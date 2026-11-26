//! User-editable settings, persisted to `ux0:data/xcloud-rust/settings.json` on each change.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const SETTINGS_DIR: &str = "ux0:data/xcloud-rust";
const SETTINGS_PATH: &str = "ux0:data/xcloud-rust/settings.json";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Locale {
    #[default]
    EnUs,
    EnGb,
    PtBr,
    PtPt,
    EsEs,
    EsMx,
    FrFr,
    DeDe,
    ItIt,
    JaJp,
    KoKr,
    ZhCn,
    ZhTw,
    RuRu,
    PlPl,
    NlNl,
    SvSe,
    TrTr,
    ArSa,
}

impl Locale {
    pub const ALL: [Locale; 19] = [
        Self::EnUs,
        Self::EnGb,
        Self::PtBr,
        Self::PtPt,
        Self::EsEs,
        Self::EsMx,
        Self::FrFr,
        Self::DeDe,
        Self::ItIt,
        Self::JaJp,
        Self::KoKr,
        Self::ZhCn,
        Self::ZhTw,
        Self::RuRu,
        Self::PlPl,
        Self::NlNl,
        Self::SvSe,
        Self::TrTr,
        Self::ArSa,
    ];

    /// `(locale code, store market, native-language label)`.
    fn info(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::EnUs => ("en-US", "US", "English (US)"),
            Self::EnGb => ("en-GB", "GB", "English (UK)"),
            Self::PtBr => ("pt-BR", "BR", "Português (Brasil)"),
            Self::PtPt => ("pt-PT", "PT", "Português (Portugal)"),
            Self::EsEs => ("es-ES", "ES", "Español (España)"),
            Self::EsMx => ("es-MX", "MX", "Español (México)"),
            Self::FrFr => ("fr-FR", "FR", "Français"),
            Self::DeDe => ("de-DE", "DE", "Deutsch"),
            Self::ItIt => ("it-IT", "IT", "Italiano"),
            Self::JaJp => ("ja-JP", "JP", "日本語"),
            Self::KoKr => ("ko-KR", "KR", "한국어"),
            Self::ZhCn => ("zh-CN", "CN", "中文（简体）"),
            Self::ZhTw => ("zh-TW", "TW", "中文（繁體）"),
            Self::RuRu => ("ru-RU", "RU", "Русский"),
            Self::PlPl => ("pl-PL", "PL", "Polski"),
            Self::NlNl => ("nl-NL", "NL", "Nederlands"),
            Self::SvSe => ("sv-SE", "SE", "Svenska"),
            Self::TrTr => ("tr-TR", "TR", "Türkçe"),
            Self::ArSa => ("ar-SA", "SA", "العربية"),
        }
    }

    /// The locale code sent to xCloud, e.g. `"pt-BR"`.
    pub fn as_str(self) -> &'static str {
        self.info().0
    }

    /// The Microsoft Store catalog's `market` query param, e.g. `"BR"`.
    pub fn market(self) -> &'static str {
        self.info().1
    }

    /// Display label in the language's own native name, e.g. `"Русский"`.
    pub fn label(self) -> &'static str {
        self.info().2
    }
}

/// Upper bound advertised to xCloud in the client capabilities message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MaxBitrate {
    #[default]
    Mbps2 = 2,
    Mbps3 = 3,
    Mbps4 = 4,
    Mbps5 = 5,
    Mbps6 = 6,
    Mbps8 = 8,
    // Legacy settings saved with removed options (15–50 Mbps) deserialize to this variant.
    #[serde(other)]
    Mbps10 = 10,
}

impl MaxBitrate {
    pub const ALL: [MaxBitrate; 7] = [
        Self::Mbps2,
        Self::Mbps3,
        Self::Mbps4,
        Self::Mbps5,
        Self::Mbps6,
        Self::Mbps8,
        Self::Mbps10,
    ];

    /// Display label, e.g. `"8 Mbps"`.
    pub fn label(self) -> &'static str {
        match self {
            Self::Mbps2 => "2 Mbps",
            Self::Mbps3 => "3 Mbps",
            Self::Mbps4 => "4 Mbps",
            Self::Mbps5 => "5 Mbps",
            Self::Mbps6 => "6 Mbps",
            Self::Mbps8 => "8 Mbps",
            Self::Mbps10 => "10 Mbps",
        }
    }

    /// Value for the `maxBitrateKbps` capability field.
    pub fn kbps(self) -> u32 {
        self as u32 * 1000
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub locale: Locale,
    /// Advertised to xCloud when a new stream starts; changing it doesn't affect a running stream.
    pub max_bitrate: MaxBitrate,
    /// Shows internal stream/session state on the `Streaming` screen. Off by default.
    pub show_stream_debug_info: bool,
    /// Writes the 1 Hz status line of every session to
    /// `ux0:data/xcloud-rust/stream.log` when the session ends.
    ///
    /// The file is written once, at the end of the session, so it costs
    /// nothing while the stream is running. It is the trajectory of the
    /// session, which is what tells whether the picture falls behind and
    /// where the time goes - so it is off by default, and this turns it on
    /// for a session that is being looked into.
    pub log_stream: bool,
    pub game_profiles: HashMap<String, GameProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct GameProfile {
    pub swap_shoulders_and_triggers: bool,
    /// When enabled, front touch mirrors the rear L2/R2/L3/R3 zones instead of acting as a
    /// clickable xCloud pointer. Defaults to pointer mode for existing profiles.
    pub front_touch_auxiliary_buttons: bool,
    /// Controls only input from the physical rear touch panel. Front touch remains independent.
    pub rear_touch_enabled: bool,
}

impl Default for GameProfile {
    fn default() -> Self {
        Self {
            swap_shoulders_and_triggers: false,
            front_touch_auxiliary_buttons: false,
            rear_touch_enabled: true,
        }
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            locale: Locale::default(),
            max_bitrate: MaxBitrate::default(),
            show_stream_debug_info: false,
            log_stream: true,
            game_profiles: HashMap::new(),
        }
    }
}

impl Settings {
    pub fn game_profile(&self, title_id: &str) -> Option<&GameProfile> {
        self.game_profiles.get(title_id)
    }

    pub fn set_swap_shoulders_and_triggers(&mut self, title_id: String, enabled: bool) {
        self.game_profiles
            .entry(title_id)
            .or_default()
            .swap_shoulders_and_triggers = enabled;
    }

    pub fn set_front_touch_auxiliary_buttons(&mut self, title_id: String, enabled: bool) {
        self.game_profiles
            .entry(title_id)
            .or_default()
            .front_touch_auxiliary_buttons = enabled;
    }

    pub fn set_rear_touch_enabled(&mut self, title_id: String, enabled: bool) {
        self.game_profiles
            .entry(title_id)
            .or_default()
            .rear_touch_enabled = enabled;
    }

    /// Loads from disk, falling back to defaults on any error.
    pub fn load() -> Self {
        let data = match std::fs::read_to_string(SETTINGS_PATH) {
            Ok(data) => data,
            Err(error) => {
                eprintln!("Settings: no existing {SETTINGS_PATH} ({error}), using defaults");
                return Self::default();
            }
        };
        match serde_json::from_str(&data) {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!("Settings: failed to parse {SETTINGS_PATH} ({error}), using defaults");
                Self::default()
            }
        }
    }

    /// Saves to disk; errors are logged rather than surfaced.
    pub fn save(&self) {
        let result = std::fs::create_dir_all(SETTINGS_DIR)
            .context("failed to create settings directory")
            .and_then(|_| {
                serde_json::to_string_pretty(self).context("failed to serialize settings")
            })
            .and_then(|data| crate::fs_utils::write_file_truncating(SETTINGS_PATH, data));

        if let Err(error) = result {
            eprintln!("Settings: failed to save: {error:#}");
        }
    }
}
