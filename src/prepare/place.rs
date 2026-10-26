//! The places a transfer's sides run at: this machine, or a host as the user
//! named it. Each is reached once, however many sides run there: two sides
//! on one host share its SSH connections, and its sudo. Two names for one
//! machine are two places.

use std::sync::{Arc, Mutex};

use anyhow::Result;

use super::probe::Probe;
use crate::monitor::client::Node;
use crate::remote::{Location, Sudo};
use crate::transport::monitor::Monitor;
use crate::transport::{FirstSession, Transport};

/// Where a side runs: this machine, or a host as the user named it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PlaceKey {
    Local,
    Host(String),
}

impl PlaceKey {
    pub fn of(location: &Location) -> Self {
        match location {
            Location::Local(_) => Self::Local,
            Location::Remote { host, .. } => Self::Host(host.clone()),
        }
    }
}

/// One place, and what preparing has made ready there so far.
pub struct Place {
    pub key: PlaceKey,
    /// Shared with the sides that run there, which keep it open until the
    /// transfer is over.
    pub transport: Arc<Transport>,
    /// On a host, what its probe found, once it is probed.
    pub probe: Option<Probe>,
    /// On a host, its first session, once it is probed, until it becomes
    /// the monitor: behind a lock only so that a place may be shared.
    pub session: Mutex<Option<FirstSession>>,
    /// On a host, Ferry's monitor there, once it is followed: what starts
    /// its servers.
    pub monitor: Option<Arc<Monitor>>,
    /// The Ferry program its servers run, once it is ready: on a host, once
    /// installed.
    pub program: Option<String>,
    /// With `--remote-sudo`, the sudo a host's servers run through, once it
    /// is ready.
    pub sudo: Option<Sudo>,
}

impl Place {
    /// Opens the transport to a location's place: for a host, logs in to
    /// its SSH connection, the host a scope of the client's monitor under
    /// `under`.
    pub fn open(location: &Location, under: &Node) -> Result<Self> {
        Ok(Self {
            key: PlaceKey::of(location),
            transport: Arc::new(Transport::open(location, under)?),
            probe: None,
            session: Mutex::new(None),
            monitor: None,
            program: None,
            sudo: None,
        })
    }
}

/// A transfer's places, in the order its sides first name them.
pub struct Places(Vec<Place>);

impl Places {
    /// Opens the place of each location, once each, in order, under
    /// `under`.
    pub fn open(locations: &[&Location], under: &Node) -> Result<Self> {
        let mut places: Vec<Place> = Vec::new();
        for location in locations {
            let key = PlaceKey::of(location);
            if places.iter().all(|place| place.key != key) {
                places.push(Place::open(location, under)?);
            }
        }
        Ok(Self(places))
    }

    /// The place `location` is at.
    pub fn of(&self, location: &Location) -> &Place {
        let key = PlaceKey::of(location);
        self.0
            .iter()
            .find(|place| place.key == key)
            .expect("every side's place is opened")
    }

    /// Every place, in order.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut Place> {
        self.0.iter_mut()
    }
}
