//! Background preview loading and an LRU texture cache.
//!
//! Decoding a 24 MP JPEG takes long enough to stutter the UI, so previews are
//! generated on a dedicated thread and uploaded as textures on the UI thread.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

use eframe::egui::{ColorImage, Context, TextureHandle, TextureOptions};
use image::RgbaImage;

use crate::imaging;
use crate::model::PhotoRecord;

struct Done {
    key: String,
    result: Result<RgbaImage, String>,
}

struct Request {
    key: String,
    path: String,
    max_dim: u32,
}

struct Cached {
    texture: TextureHandle,
    last_used: Instant,
}

pub(crate) struct ImageCache {
    tx: Sender<Request>,
    rx: Receiver<Done>,
    pending: HashSet<String>,
    cache: HashMap<String, Cached>,
    order: VecDeque<String>,
    errors: HashMap<String, String>,
}

impl ImageCache {
    pub(crate) fn new() -> Self {
        let (req_tx, req_rx) = mpsc::channel::<Request>();
        let (done_tx, done_rx) = mpsc::channel::<Done>();
        std::thread::Builder::new()
            .name("pixsort-previews".into())
            .spawn(move || {
                while let Ok(request) = req_rx.recv() {
                    let result =
                        imaging::load_preview(std::path::Path::new(&request.path), request.max_dim)
                            .map_err(|err| format!("{err:#}"));
                    if done_tx
                        .send(Done {
                            key: request.key,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .ok();
        Self {
            tx: req_tx,
            rx: done_rx,
            pending: HashSet::new(),
            cache: HashMap::new(),
            order: VecDeque::new(),
            errors: HashMap::new(),
        }
    }

    fn key(photo: &PhotoRecord, max_dim: u32) -> String {
        format!("{}|{}|{}", photo.path, photo.mtime_ns, max_dim)
    }

    /// Returns a texture immediately if cached, otherwise starts loading.
    pub(crate) fn texture(
        &mut self,
        ctx: &Context,
        photo: &PhotoRecord,
        max_dim: u32,
    ) -> Option<TextureHandle> {
        let key = Self::key(photo, max_dim);
        if let Some(cached) = self.cache.get_mut(&key) {
            cached.last_used = Instant::now();
            self.order.retain(|k| k != &key);
            self.order.push_back(key.clone());
            return Some(cached.texture.clone());
        }
        if self.errors.contains_key(&key) {
            return None;
        }
        if self.pending.insert(key.clone()) {
            let request = Request {
                key: key.clone(),
                path: photo.path.clone(),
                max_dim,
            };
            if self.tx.send(request).is_err() {
                self.pending.remove(&key);
                self.errors.insert(key, "preview thread stopped".into());
            } else {
                ctx.request_repaint();
            }
        }
        None
    }

    pub(crate) fn error_for(&self, photo: &PhotoRecord, max_dim: u32) -> Option<&str> {
        self.errors
            .get(&Self::key(photo, max_dim))
            .map(|s| s.as_str())
    }

    pub(crate) fn poll(&mut self, ctx: &Context) -> bool {
        let mut changed = false;
        while let Ok(done) = self.rx.try_recv() {
            self.pending.remove(&done.key);
            changed = true;
            match done.result {
                Ok(image) => {
                    let size = [image.width() as usize, image.height() as usize];
                    let color = ColorImage::from_rgba_unmultiplied(size, image.as_raw());
                    let texture = ctx.load_texture(
                        format!("preview:{}", done.key),
                        color,
                        TextureOptions::LINEAR,
                    );
                    self.cache.insert(
                        done.key.clone(),
                        Cached {
                            texture,
                            last_used: Instant::now(),
                        },
                    );
                    self.errors.remove(&done.key);
                    self.order.retain(|k| k != &done.key);
                    self.order.push_back(done.key);
                }
                Err(message) => {
                    self.errors.insert(done.key, message);
                }
            }
        }
        while self.cache.len() > 16 {
            if let Some(oldest) = self.order.pop_front() {
                self.cache.remove(&oldest);
            } else {
                break;
            }
        }
        if changed {
            ctx.request_repaint();
        }
        changed
    }

    pub(crate) fn clear(&mut self) {
        self.cache.clear();
        self.order.clear();
        self.errors.clear();
    }
}
