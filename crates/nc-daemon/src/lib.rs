// SPDX-FileCopyrightText: 2026 nextcloud-client-rs contributors
// SPDX-License-Identifier: GPL-2.0-or-later

//! Daemon side of the sync client: what the official desktop client does in
//! `src/gui` without a GUI (folder manager, folders, folder watcher, account
//! state, configuration, credentials).

pub mod account_config;
pub mod account_setup;
pub mod account_state;
pub mod config_file;
pub mod connection_validator;
pub mod control;
pub mod credentials;
pub mod daemon;
pub mod event;
pub mod flow2auth;
pub mod folder;
pub mod folder_definition;
pub mod folder_man;
pub mod folder_watcher;
pub mod instance;
pub mod manage;
pub mod network_limits;
pub mod remote_wipe;
pub mod selective_sync;
pub mod settings;
pub mod startup;
pub mod sync_result;
pub mod takeover;
pub mod timer;
