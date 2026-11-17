//! Состояние фоновых потоков, которое показывается в меню трея.

use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Engine {
    #[default]
    Loading,
    Ready,
    Error(String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Obs {
    #[default]
    Off,
    Connecting,
    Connected,
    Error(String),
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub engine: Engine,
    pub obs: Obs,
    /// Найдена новая версия.
    pub update: Option<String>,
}

pub type SharedStatus = Arc<Mutex<Status>>;
