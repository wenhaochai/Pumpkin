//! The files of a Pumpkin install, the unit that a release archives and a controller installs
//! whole: `pumpkin` and the files beside it, in one directory, `bin/`. `cargo xtask build` lays
//! the install out, and `pumpkin` sends its own to a controller.
#![no_std]

/// A file of an install, in its `bin/`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum File {
    Pumpkin,
    Buildifier,
    Ec2Boot,
    Sandboxd,
    /// The Modal provider's helper, a script that `uv` runs.
    ModalHelper,
    /// The lock of the helper's packages.
    ModalLock,
}

impl File {
    /// Every file of an install.
    pub const ALL: [Self; 6] = [
        Self::Pumpkin,
        Self::Buildifier,
        Self::Ec2Boot,
        Self::Sandboxd,
        Self::ModalHelper,
        Self::ModalLock,
    ];

    /// Its name in `bin/`, by which `pumpkin` finds it beside itself.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pumpkin => "pumpkin",
            Self::Buildifier => "buildifier",
            Self::Ec2Boot => "ec2-boot",
            Self::Sandboxd => "sandboxd",
            Self::ModalHelper => "pumpkin-modal.py",
            Self::ModalLock => "pumpkin-modal.py.lock",
        }
    }

    /// Whether it is a static executable, which an install keeps executable; the others are read
    /// by other programs.
    pub const fn is_executable(self) -> bool {
        match self {
            Self::Pumpkin | Self::Buildifier | Self::Ec2Boot | Self::Sandboxd => true,
            Self::ModalHelper | Self::ModalLock => false,
        }
    }
}
