//! This technology's declaration: the transport guide's "The declaration",
//! `module/core/capability/transport/doc/adding-a-transport.md`.

use xcore::settings::{Applies, Fixed, Kind, Presence, Setting, Settings};

/// The session id a select is made under when the equipment has not said.
pub const ANY_SESSION: u16 = 0xFFFF;

/// Every setting a Location takes beyond its address, where a Receive
/// Location listens as the passive side, `0.0.0.0:5000`; a Send Location's
/// target is each send's own.
pub const SETTINGS: &Settings = &Settings {
    technology: env!("CARGO_PKG_NAME"),
    settings: &[
        Setting {
            name: "session",
            kind: Kind::Integer {
                minimum: 0,
                maximum: 65_535,
            },
            presence: Presence::Default(Fixed::Integer(ANY_SESSION as i64)),
            meaning: "The session id, the device id, a Send Location selects under.",
            applies: Applies::Send,
        },
        Setting {
            name: "wait",
            kind: Kind::Boolean,
            presence: Presence::Optional,
            meaning: "Whether what is sent sets the wait bit and takes the reply, unless the \
                      target says `wait=0`; no reply is waited for when left out.",
            applies: Applies::Send,
        },
        transport::configured::timeout(
            Applies::Both,
            None,
            "How long a peer that stops mid-message or never replies is waited on; \
             unbounded when left out.",
        ),
    ],
};

/// This technology's schemes, as [`transport::Configured::SCHEMES`] defines
/// them.
pub const SCHEMES: &[&str] = &["secs-gem"];
