/// The key a game reads its hand-off under: `Launch Options/<codename>/…` in
/// the `net.battle` preferences. Also the scope of a launch token: one token
/// per account and codename. Every WoW flavor uses `WoW`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GameCodename(&'static str);

impl GameCodename {
    pub const WOW: GameCodename = GameCodename("WoW");

    pub fn as_str(self) -> &'static str {
        self.0
    }
}

/// A game StoneForm installs and launches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Product {
    /// TACT product code, used by the version service and in `.build.info`.
    pub code: &'static str,
    /// Battle.net install uid; also what `-uid` passes to the game.
    pub uid: &'static str,
    pub codename: GameCodename,
    pub display_name: &'static str,
    /// The install root every WoW flavor shares.
    pub folder_name: &'static str,
    /// This flavor's folder inside the root; the game runs from here.
    pub flavor_folder: &'static str,
    /// The app bundle, relative to the install root.
    pub app_path: &'static str,
    /// Arguments before `-uid <uid>`.
    pub launch_arguments: &'static [&'static str],
    /// The game languages it can be installed in; the product config has the live list.
    pub locales: &'static [&'static str],
}

/// WoW: Forever, as Waypoint's catalog tests record it (upstream e566728).
pub const WOW_FOREVER: Product = Product {
    code: "wowf",
    uid: "wowf",
    codename: GameCodename::WOW,
    display_name: "WoW: Forever",
    folder_name: "World of Warcraft",
    flavor_folder: "_forever_",
    app_path: "_forever_/World of Warcraft.app",
    launch_arguments: &["-launcherlogin"],
    locales: &["enUS", "deDE", "esES", "esMX", "frFR", "ruRU", "koKR", "ptBR", "zhTW", "zhCN"],
};
