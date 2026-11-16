//! The settings snapshot read from the shared config database, and the writing back of an edit.
//!
//! An edit is written to the same row the egui app reads, so a setting changed in any of
//! them holds in all of them.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use tokio::runtime::Runtime;
use tokio_util::task::TaskTracker;
use wows_core::pseudonym::NameDisplay;
use wows_toolkit_config::queries::ArmorViewerDefaultsRow;
use wows_toolkit_services::settings::StartupSettings;
use wows_toolkit_services::settings_writer::PendingWrite;
use wows_toolkit_services::settings_writer::SettingsWriter;
use zgui::reactive::LocalStorage;
use zgui::reactive::RwSignal;
use zgui::reactive::StoredValue;
use zgui::reactive::prelude::GetValue;
use zgui::reactive::prelude::Set;
use zgui::reactive::prelude::SetValue;
use zgui::reactive::prelude::Update;
use zgui::reactive::prelude::With;
use zgui::reactive::prelude::WithUntracked;

/// Where the settings read has got.
pub enum SettingsState {
    Loading,
    /// Boxed: the settings are far larger than the other variants.
    Loaded(Box<StartupSettings>),
    /// The database could not be opened or read; the reason is what the drawer says.
    Failed(String),
}

/// The settings, and where an edit to them is written.
#[derive(Clone, Copy)]
pub struct Settings {
    state: RwSignal<SettingsState, LocalStorage>,
    /// `None` until the startup read has opened the database.
    writer: StoredValue<Option<SettingsWriter>>,
    runtime: StoredValue<Option<Arc<Runtime>>>,
}

impl Settings {
    pub(super) fn new(runtime: StoredValue<Option<Arc<Runtime>>>) -> Self {
        Self { state: RwSignal::new_local(SettingsState::Loading), writer: StoredValue::new(None), runtime }
    }

    /// Takes the startup read, and the pool later edits are written through.
    pub fn adopt(self, settings: StartupSettings, pool: SqlitePool) {
        self.writer.set_value(Some(SettingsWriter::new(pool)));
        self.state.set(SettingsState::Loaded(Box::new(settings)));
    }

    /// Takes settings that have no database behind them, as a test does.
    #[cfg(test)]
    pub fn adopt_unsaved(self, settings: StartupSettings) {
        self.state.set(SettingsState::Loaded(Box::new(settings)));
    }

    /// Writes this window's saves to an empty database of its own: a test that changes a setting
    /// or the session must not change what other windows in the process read.
    #[cfg(test)]
    pub fn use_private_database_for_test(self) {
        let runtime = self.runtime.get_value().expect("the test runtime is up");
        let pool = runtime.block_on(wows_toolkit_config::test_pool());
        self.writer.set_value(Some(SettingsWriter::new(pool)));
    }

    pub fn fail(self, reason: String) {
        self.state.set(SettingsState::Failed(reason));
    }

    /// Reads the settings, subscribing to every change. `None` while they are not loaded.
    pub fn read<R>(self, read: impl FnOnce(&StartupSettings) -> R) -> Option<R> {
        self.state.with(|state| match state {
            SettingsState::Loaded(settings) => Some(read(settings)),
            SettingsState::Loading | SettingsState::Failed(_) => None,
        })
    }

    /// [`Self::read`] without subscribing.
    pub fn read_untracked<R>(self, read: impl FnOnce(&StartupSettings) -> R) -> Option<R> {
        self.state.with_untracked(|state| match state {
            SettingsState::Loaded(settings) => Some(read(settings)),
            SettingsState::Loading | SettingsState::Failed(_) => None,
        })
    }

    /// Whether the read has finished, and how.
    pub fn with_state<R>(self, read: impl FnOnce(&SettingsState) -> R) -> R {
        self.state.with(read)
    }

    /// Whether the read is still running, without subscribing.
    pub fn is_loading_untracked(self) -> bool {
        self.state.with_untracked(|state| matches!(state, SettingsState::Loading))
    }

    /// How player names and clan tags are shown, subscribing to changes. Real names until the
    /// settings are loaded, which is the default the setting itself has.
    pub fn name_display(self) -> NameDisplay {
        self.read(|settings| NameDisplay::from_anonymize(settings.replay.anonymize_player_names))
            .unwrap_or(NameDisplay::Real)
    }

    /// [`Self::name_display`] without subscribing.
    pub fn name_display_untracked(self) -> NameDisplay {
        self.read_untracked(|settings| NameDisplay::from_anonymize(settings.replay.anonymize_player_names))
            .unwrap_or(NameDisplay::Real)
    }

    /// Changes the loaded settings with `apply`, and saves what it returns under `key`. Nothing
    /// happens while the settings are not loaded, since there is nothing to change yet.
    pub fn edit<T: Serialize>(self, key: &'static str, apply: impl FnOnce(&mut StartupSettings) -> T) {
        let mut value = None;
        self.state.update(|state| {
            if let SettingsState::Loaded(settings) = state {
                value = Some(apply(settings));
            }
        });

        if let Some(value) = value {
            self.save(key, &value);
        }
    }

    /// Writes `value` under `key`.
    ///
    /// Fire and forget: the control has already moved, and a failed write is logged rather than
    /// shown, as the other front ends do. Saves of one key commit in the order they were made.
    pub fn save<T: Serialize>(self, key: &'static str, value: &T) {
        let json = match serde_json::to_string(value) {
            Ok(json) => json,
            Err(err) => {
                tracing::error!("settings: {key} could not be serialized: {err}");
                return;
            }
        };

        #[cfg(test)]
        SAVED.with(|saved| saved.borrow_mut().push((key, json.clone())));

        let Some(writer) = self.writer.get_value() else {
            tracing::warn!("settings: {key} was not saved because the config database is not open");
            return;
        };

        self.commit_detached(writer.begin(key, json));
    }

    /// Writes the armor viewer's defaults row, which is a table of its own rather than a key, and
    /// keeps the snapshot in step with it. Saves of the row commit in the order they were made.
    pub fn save_armor_defaults(self, row: ArmorViewerDefaultsRow) {
        self.state.update(|state| {
            if let SettingsState::Loaded(settings) = state {
                settings.armor_defaults = Some(row.clone());
            }
        });

        #[cfg(test)]
        SAVED.with(|saved| saved.borrow_mut().push((ARMOR_DEFAULTS, format!("{row:?}"))));

        let Some(writer) = self.writer.get_value() else {
            tracing::warn!("settings: the armor viewer defaults were not saved because the database is not open");
            return;
        };

        self.commit_detached(writer.begin_armor_defaults(row));
    }

    /// Commits `pending` once its turn comes, unless a newer save has overtaken it.
    ///
    /// On the tokio runtime rather than the UI executor, and tracked: a save must land even when
    /// the view that made it goes away first, and when the app quits straight after it
    /// ([`finish_pending_writes`]).
    fn commit_detached(self, pending: PendingWrite) {
        let key = pending.key();
        let Some(runtime) = self.runtime.get_value() else {
            tracing::error!("settings: {key} was not written because no tokio runtime is running");
            return;
        };

        runtime.spawn(WRITES_IN_FLIGHT.track_future(async move {
            let Some(write) = pending.turn().await else { return };
            if let Err(err) = write.commit().await {
                tracing::error!("settings: {key} could not be written: {err}");
            }
        }));
    }

    /// The open config pool, for the readers that query it directly. `None` before the startup
    /// read has opened it, and once the services are gone: a secondary window can outlive the main
    /// window that owns them.
    pub fn pool(self) -> Option<SqlitePool> {
        self.writer.try_get_value().flatten().map(|writer| writer.pool().clone())
    }
}

/// The settings writes not yet committed. Process-wide: the services that start them are gone by
/// the time the window loop returns, and the writes have to outlive them.
static WRITES_IN_FLIGHT: std::sync::LazyLock<TaskTracker> = std::sync::LazyLock::new(TaskTracker::new);

/// Waits up to `timeout` for every settings write already started to commit. Called once the
/// window loop has returned, before the runtime is dropped and aborts them.
pub fn finish_pending_writes(runtime: &Runtime, timeout: Duration) {
    WRITES_IN_FLIGHT.close();
    // Built inside the runtime: a tokio timer made outside one panics.
    if runtime.block_on(async { tokio::time::timeout(timeout, WRITES_IN_FLIGHT.wait()).await }).is_err() {
        tracing::warn!("settings: {} writes were still pending at exit and are lost", WRITES_IN_FLIGHT.len());
    }
}

/// What [`saved_for_test`] names a write of the armor viewer defaults row.
#[cfg(test)]
pub const ARMOR_DEFAULTS: &str = "armor_viewer_defaults";

#[cfg(test)]
thread_local! {
    /// Every save asked for on this thread, as its key and its JSON: a test window has no database
    /// to read them back from.
    static SAVED: std::cell::RefCell<Vec<(&'static str, String)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Every save asked for on this thread so far, oldest first.
#[cfg(test)]
pub fn saved_for_test() -> Vec<(&'static str, String)> {
    SAVED.with(|saved| saved.borrow().clone())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use wows_toolkit_services::settings::StartupSettings;
    use zgui::reactive::Mounted;

    use super::finish_pending_writes;
    use crate::services::Services;

    /// A setting changed just before the app quits is in the database once the exit has waited
    /// for it, though the window and its executor are already gone.
    #[test]
    fn a_write_made_just_before_exit_lands() {
        const KEY: &str = "zgui_test_written_at_exit";

        zgui::reactive::install().expect("the reactive runtime installs");
        let runtime = Arc::new(wows_toolkit_services::runtime::build().expect("a tokio runtime starts"));
        let main = Mounted::new();
        let services = main.with(|| Services::provide(Some(Arc::clone(&runtime))));
        let pool = runtime.block_on(wows_toolkit_config::test_pool());
        let settings = runtime.block_on(StartupSettings::load(&pool));
        services.settings.adopt(settings, pool.clone());

        services.settings.save(KEY, &"quit");
        main.unmount();
        finish_pending_writes(&runtime, Duration::from_secs(5));

        let stored = runtime.block_on(wows_toolkit_config::queries::get_setting::<String>(&pool, KEY));
        assert_eq!(stored.as_deref(), Some("quit"));
    }
}
