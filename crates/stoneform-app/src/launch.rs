//! The `Launch` use case: the token to the game, then the game.

use std::path::Path;

use stoneform_domain::{AccountId, Locale, Product, Region};

use crate::ports::{GameProcess, Running, SecretError, SecretStore, SystemError, TokenHandoff};

/// Where and as whom to start the game.
#[derive(Debug, Clone, Copy)]
pub struct LaunchTarget<'a> {
    /// The `World of Warcraft` folder.
    pub root: &'a Path,
    pub product: &'a Product,
    pub account: &'a AccountId,
    pub region: Region,
    /// Written only when known, so a language picked in Battle.net survives.
    pub locale: Option<&'a Locale>,
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("there is no saved sign-in for this game; sign in again")]
    SignInRequired,
    #[error(transparent)]
    Secrets(#[from] SecretError),
    #[error(transparent)]
    System(#[from] SystemError),
}

#[derive(Debug)]
pub struct Launcher<S, H, P> {
    secrets: S,
    handoff: H,
    processes: P,
}

impl<S: SecretStore, H: TokenHandoff, P: GameProcess> Launcher<S, H, P> {
    pub fn new(secrets: S, handoff: H, processes: P) -> Self {
        Self { secrets, handoff, processes }
    }

    pub fn running(&self, root: &Path) -> Running {
        self.processes.running(root)
    }

    pub fn bring_forward(&self, pid: u32) -> bool {
        self.processes.activate(pid)
    }

    /// Hands the account's token to the game and starts it from its flavor
    /// folder, as Battle.net does: `World of Warcraft -launcherlogin -uid wowf`.
    /// Returns the game's process id; the game outlives StoneForm.
    pub fn launch(&self, target: LaunchTarget<'_>) -> Result<u32, LaunchError> {
        let product = target.product;
        let token = self.secrets.token(target.account, product.codename)?.ok_or(LaunchError::SignInRequired)?;
        self.handoff.write(product.codename, &token, target.region, target.locale)?;
        let arguments: Vec<String> = product
            .launch_arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .chain(["-uid".to_owned(), product.uid.to_owned()])
            .collect();
        let app = target.root.join(product.app_path);
        let pid = self.processes.spawn(&app, &arguments, &target.root.join(product.flavor_folder))?;
        tracing::info!(pid, account = %target.account, region = %target.region, "game started");
        Ok(pid)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use stoneform_domain::{GameCodename, LaunchToken, WOW_FOREVER};

    use super::*;

    const TOKEN: &str = "EU-0123456789abcdef0123456789abcdef-42";

    struct OneToken(Option<LaunchToken>);

    impl SecretStore for OneToken {
        fn has_token(&self, _: &AccountId, _: GameCodename) -> Result<bool, SecretError> {
            Ok(self.0.is_some())
        }
        fn token(&self, _: &AccountId, _: GameCodename) -> Result<Option<LaunchToken>, SecretError> {
            Ok(self.0.clone())
        }
        fn save(&self, _: &LaunchToken, _: GameCodename) -> Result<(), SecretError> {
            unreachable!()
        }
        fn forget(&self, _: &AccountId) -> Result<(), SecretError> {
            unreachable!()
        }
    }

    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl TokenHandoff for Recorder {
        fn write(
            &self,
            codename: GameCodename,
            token: &LaunchToken,
            region: Region,
            locale: Option<&Locale>,
        ) -> Result<(), SystemError> {
            let locale = locale.map(Locale::as_str).unwrap_or("-");
            self.0.lock().unwrap().push(format!("{} {} {region} {locale}", codename.as_str(), token.expose()));
            Ok(())
        }
    }

    impl GameProcess for Recorder {
        fn running(&self, _: &Path) -> Running {
            Running::default()
        }
        fn spawn(&self, app: &Path, arguments: &[String], cwd: &Path) -> Result<u32, SystemError> {
            self.0.lock().unwrap().push(format!("{} {} in {}", app.display(), arguments.join(" "), cwd.display()));
            Ok(4242)
        }
        fn activate(&self, _: u32) -> bool {
            true
        }
    }

    fn target<'a>(root: &'a Path, account: &'a AccountId, locale: Option<&'a Locale>) -> LaunchTarget<'a> {
        LaunchTarget { root, product: &WOW_FOREVER, account, region: Region::Eu, locale }
    }

    #[test]
    fn hands_off_the_token_then_starts_the_game_from_its_flavor_folder() {
        let token = LaunchToken::parse(TOKEN).unwrap();
        let account = token.account_id();
        let launcher = Launcher::new(OneToken(Some(token)), Recorder::default(), Recorder::default());
        let root = PathBuf::from("/Applications/World of Warcraft");
        let locale = Locale::parse("deDE").unwrap();

        let pid = launcher.launch(target(&root, &account, Some(&locale))).unwrap();

        assert_eq!(pid, 4242);
        assert_eq!(*launcher.handoff.0.lock().unwrap(), vec![format!("WoW {TOKEN} eu deDE")]);
        assert_eq!(
            *launcher.processes.0.lock().unwrap(),
            vec![
                "/Applications/World of Warcraft/_forever_/World of Warcraft.app -launcherlogin -uid wowf \
                 in /Applications/World of Warcraft/_forever_"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn asks_for_a_sign_in_when_there_is_no_token_and_starts_nothing() {
        let account = AccountId::parse("42").unwrap();
        let launcher = Launcher::new(OneToken(None), Recorder::default(), Recorder::default());
        let root = PathBuf::from("/Applications/World of Warcraft");

        let error = launcher.launch(target(&root, &account, None)).unwrap_err();

        assert!(matches!(error, LaunchError::SignInRequired));
        assert!(launcher.handoff.0.lock().unwrap().is_empty());
        assert!(launcher.processes.0.lock().unwrap().is_empty());
    }
}
