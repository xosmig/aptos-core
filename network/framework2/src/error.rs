// Copyright © Aptos Foundation
// Parts of the project are originally copyright © Meta Platforms, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::error::Error;
use std::fmt::{Display, Formatter};
use crate::{application};  // peer_manager::PeerManagerError
use std::io;
use thiserror::Error;

// #[derive(Copy, Clone, Eq, PartialEq, Debug, Error)]
// #[error(transparent)]


/// Errors propagated from the network module.
#[derive(Debug)]
pub enum NetworkError {
    Error(String),

    // #[error("IO error")]
    IoError(io::Error),

    // #[error("Bcs error")]
    BcsError(bcs::Error),

    // #[error("Peer full")]
    PeerFullCondition,

    // #[error("Peer not connected")]
    NotConnected,
}

// impl From<NetworkErrorKind> for NetworkError {
//     fn from(kind: NetworkErrorKind) -> NetworkError {
//         NetworkError(anyhow::Error::new(kind))
//     }
// }

impl From<anyhow::Error> for NetworkError {
    fn from(err: anyhow::Error) -> NetworkError {
        NetworkError::Error(err.to_string())
    }
}

impl From<String> for NetworkError {
    fn from(err: String) -> NetworkError {
        NetworkError::Error(err)
    }
}

impl From<io::Error> for NetworkError {
    fn from(err: io::Error) -> NetworkError {
        // anyhow::Error::new(err)
        //     .context(NetworkErrorKind::IoError)
        //     .into()
        NetworkError::IoError(err)
    }
}

impl From<bcs::Error> for NetworkError {
    fn from(err: bcs::Error) -> NetworkError {
        // anyhow::Error::new(err)
        //     .context(NetworkErrorKind::BcsError)
        //     .into()
        NetworkError::BcsError(err)
    }
}

// impl From<PeerManagerError> for NetworkError {
//     fn from(err: PeerManagerError) -> NetworkError {
//         match err {
//             PeerManagerError::IoError(_) => anyhow::Error::new(err)
//                 .context(NetworkErrorKind::IoError)
//                 .into(),
//             PeerManagerError::NotConnected(_) => anyhow::Error::new(err)
//                 .context(NetworkErrorKind::NotConnected)
//                 .into(),
//             err => anyhow::Error::new(err)
//                 .context(NetworkErrorKind::PeerManagerError)
//                 .into(),
//         }
//     }
// }

// impl From<application::error::Error> for NetworkError {
//     fn from(err: application::error::Error) -> NetworkError {
//         anyhow::Error::new(err)
//             .context(NetworkErrorKind::IoError)
//             .into()
//     }
// }

impl Display for NetworkError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            NetworkError::Error(err_str) => {
                f.write_fmt(format_args!("NetworkError({})", err_str))
            }
            NetworkError::IoError(io_err) => {
                f.write_fmt(format_args!("NetworkError Io({})", io_err))
            }
            NetworkError::BcsError(bcs_err) => {
                f.write_fmt(format_args!("NetworkError BCS({:?})", bcs_err))
            }
            NetworkError::PeerFullCondition => {
                f.write_str("NetworkError::PeerFullCondition")
            }
            NetworkError::NotConnected => {
                f.write_str("NetworkError::NotConnected")
            }
        }
    }
}

impl Error for NetworkError {

}
