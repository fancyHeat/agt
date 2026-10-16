//! Platforms without an overlay: creating one fails, and the app runs without it.

use crate::{Error, NoticeSurface, Surface, canvas::Canvas};

pub struct Window;

impl Window {
    pub fn new() -> Result<Self, Error> {
        Err("the overlay is not supported on this platform".into())
    }
}

impl Surface for Window {
    fn place(&mut self, _: f32, _: f32, _: f32, _: bool) -> f32 {
        1.0
    }
    fn present(&mut self, _: &Canvas) {}
    fn hide(&mut self) {}
}

pub struct Notice;

impl Notice {
    pub fn new(_on_copy: Box<dyn Fn()>) -> Result<Self, Error> {
        Err("not supported on this platform".into())
    }
}

impl NoticeSurface for Notice {
    fn show(&mut self, _: f32, _: bool) {}
    fn copied(&mut self) {}
    fn hide(&mut self) {}
}
