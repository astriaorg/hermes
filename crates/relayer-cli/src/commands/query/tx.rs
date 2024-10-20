//! `query tx` subcommand

use abscissa_core::{
    clap::Parser,
    Command,
    Runnable,
};

mod events;

/// `query tx` subcommand
#[derive(Command, Debug, Parser, Runnable)]
pub enum QueryTxCmd {
    /// Query the events emitted by transaction
    Events(events::QueryTxEventsCmd),
}
