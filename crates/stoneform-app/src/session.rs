use stoneform_domain::{AccountId, GameCodename, Region, WOW_FOREVER};

use crate::ports::{LoginError, LoginFlow, SecretError, SecretStore, SettingsError, SettingsStore};

/// The `SignIn` and `SignOut` use cases, plus the check the autopilot starts with.
#[derive(Debug)]
pub struct Session<L, S, C> {
    login: L,
    secrets: S,
    settings: C,
    /// Used until the user has signed in once; then the saved region wins.
    default_region: Region,
}

/// Who is signed in, and where they play.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedIn {
    pub account: AccountId,
    pub region: Region,
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error(transparent)]
    Login(#[from] LoginError),
    #[error(transparent)]
    Secrets(#[from] SecretError),
    #[error(transparent)]
    Settings(#[from] SettingsError),
}

impl AppError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, AppError::Login(LoginError::Cancelled))
    }
}

const CODENAME: GameCodename = WOW_FOREVER.codename;

impl<L: LoginFlow, S: SecretStore, C: SettingsStore> Session<L, S, C> {
    pub fn new(login: L, secrets: S, settings: C, default_region: Region) -> Self {
        Self { login, secrets, settings, default_region }
    }

    /// The signed-in account, if its token is in the secret store. Never shows
    /// anything, so it is safe to call on every start.
    pub fn current(&self) -> Result<Option<SignedIn>, AppError> {
        let settings = self.settings.load()?;
        let Some(account) = settings.account else { return Ok(None) };
        let signed_in = self
            .secrets
            .token(&account, CODENAME)?
            .map(|_| SignedIn { account, region: settings.region.unwrap_or(self.default_region) });
        Ok(signed_in)
    }

    /// Shows the web login, then keeps the token and makes its account the
    /// signed-in one. Signing in as another account replaces the previous one.
    pub async fn sign_in(&self) -> Result<SignedIn, AppError> {
        let mut settings = self.settings.load()?;
        let region = settings.region.unwrap_or(self.default_region);
        let token = self.login.acquire(region).await?;
        let account = token.account_id();
        self.secrets.save(&token, CODENAME)?;
        if let Some(previous) = settings.account.as_ref().filter(|previous| **previous != account) {
            self.secrets.forget(previous)?;
        }
        settings.account = Some(account.clone());
        settings.region = Some(region);
        self.settings.save(&settings)?;
        tracing::info!(%account, %region, token = ?token, "signed in");
        Ok(SignedIn { account, region })
    }

    /// Forgets the token, the account and its web session. The token goes
    /// first: once it is gone, StoneForm is signed out even if a later step fails.
    pub async fn sign_out(&self) -> Result<(), AppError> {
        let mut settings = self.settings.load()?;
        let Some(account) = settings.account.take() else { return Ok(()) };
        self.secrets.forget(&account)?;
        self.settings.save(&settings)?;
        self.login.forget(&account).await?;
        tracing::info!(%account, "signed out");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use stoneform_domain::LaunchToken;

    use super::*;
    use crate::ports::Settings;

    fn token(account: &str) -> LaunchToken {
        LaunchToken::parse(&format!("EU-0123456789abcdef0123456789abcdef-{account}")).unwrap()
    }

    fn account(id: &str) -> AccountId {
        AccountId::parse(id).unwrap()
    }

    #[derive(Default)]
    struct FakeLogin {
        next: Mutex<Vec<Result<LaunchToken, LoginError>>>,
        asked_regions: Mutex<Vec<Region>>,
        forgotten: Mutex<Vec<AccountId>>,
    }

    impl FakeLogin {
        fn answering(answers: Vec<Result<LaunchToken, LoginError>>) -> Self {
            Self { next: Mutex::new(answers), ..Default::default() }
        }
    }

    impl LoginFlow for FakeLogin {
        async fn acquire(&self, region: Region) -> Result<LaunchToken, LoginError> {
            self.asked_regions.lock().unwrap().push(region);
            self.next.lock().unwrap().remove(0)
        }

        async fn forget(&self, account: &AccountId) -> Result<(), LoginError> {
            self.forgotten.lock().unwrap().push(account.clone());
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemorySecrets(Mutex<HashMap<(AccountId, GameCodename), LaunchToken>>);

    impl SecretStore for MemorySecrets {
        fn token(&self, account: &AccountId, codename: GameCodename) -> Result<Option<LaunchToken>, SecretError> {
            Ok(self.0.lock().unwrap().get(&(account.clone(), codename)).cloned())
        }

        fn save(&self, token: &LaunchToken, codename: GameCodename) -> Result<(), SecretError> {
            self.0.lock().unwrap().insert((token.account_id(), codename), token.clone());
            Ok(())
        }

        fn forget(&self, account: &AccountId) -> Result<(), SecretError> {
            self.0.lock().unwrap().retain(|(owner, _), _| owner != account);
            Ok(())
        }
    }

    #[derive(Default)]
    struct MemorySettings(Mutex<Settings>);

    impl SettingsStore for MemorySettings {
        fn load(&self) -> Result<Settings, SettingsError> {
            Ok(self.0.lock().unwrap().clone())
        }

        fn save(&self, settings: &Settings) -> Result<(), SettingsError> {
            *self.0.lock().unwrap() = settings.clone();
            Ok(())
        }
    }

    type TestSession = Session<FakeLogin, MemorySecrets, MemorySettings>;

    fn session(login: FakeLogin) -> TestSession {
        Session::new(login, MemorySecrets::default(), MemorySettings::default(), Region::Eu)
    }

    #[tokio::test]
    async fn nobody_is_signed_in_at_first() {
        let session = session(FakeLogin::default());
        assert_eq!(session.current().unwrap(), None);
    }

    #[tokio::test]
    async fn signing_in_keeps_the_token_and_the_account() {
        let session = session(FakeLogin::answering(vec![Ok(token("42"))]));

        let signed_in = session.sign_in().await.unwrap();

        let expected = SignedIn { account: account("42"), region: Region::Eu };
        assert_eq!(signed_in, expected);
        assert_eq!(session.current().unwrap(), Some(expected));
        assert_eq!(session.secrets.token(&account("42"), CODENAME).unwrap(), Some(token("42")));
        assert_eq!(*session.login.asked_regions.lock().unwrap(), vec![Region::Eu]);
    }

    #[tokio::test]
    async fn the_saved_region_wins_over_the_default() {
        let session = session(FakeLogin::answering(vec![Ok(token("42"))]));
        session.settings.save(&Settings { region: Some(Region::Kr), account: None }).unwrap();

        let signed_in = session.sign_in().await.unwrap();

        assert_eq!(signed_in.region, Region::Kr);
        assert_eq!(*session.login.asked_regions.lock().unwrap(), vec![Region::Kr]);
    }

    #[tokio::test]
    async fn a_cancelled_sign_in_changes_nothing() {
        let session = session(FakeLogin::answering(vec![Ok(token("42")), Err(LoginError::Cancelled)]));
        session.sign_in().await.unwrap();

        let error = session.sign_in().await.unwrap_err();

        assert!(error.is_cancelled());
        assert_eq!(session.current().unwrap().map(|s| s.account), Some(account("42")));
    }

    #[tokio::test]
    async fn signing_in_as_someone_else_replaces_the_previous_account() {
        let session = session(FakeLogin::answering(vec![Ok(token("42")), Ok(token("7"))]));
        session.sign_in().await.unwrap();

        session.sign_in().await.unwrap();

        assert_eq!(session.current().unwrap().map(|s| s.account), Some(account("7")));
        assert_eq!(session.secrets.token(&account("42"), CODENAME).unwrap(), None);
    }

    #[tokio::test]
    async fn a_missing_token_means_signed_out() {
        let session = session(FakeLogin::answering(vec![Ok(token("42"))]));
        session.sign_in().await.unwrap();
        session.secrets.forget(&account("42")).unwrap();

        assert_eq!(session.current().unwrap(), None);
    }

    #[tokio::test]
    async fn signing_out_forgets_token_account_and_web_session() {
        let session = session(FakeLogin::answering(vec![Ok(token("42"))]));
        session.sign_in().await.unwrap();

        session.sign_out().await.unwrap();

        assert_eq!(session.current().unwrap(), None);
        assert_eq!(session.secrets.token(&account("42"), CODENAME).unwrap(), None);
        assert_eq!(session.settings.load().unwrap().account, None);
        assert_eq!(session.settings.load().unwrap().region, Some(Region::Eu));
        assert_eq!(*session.login.forgotten.lock().unwrap(), vec![account("42")]);
    }

    #[tokio::test]
    async fn signing_out_twice_is_harmless() {
        let session = session(FakeLogin::default());
        session.sign_out().await.unwrap();
        assert!(session.login.forgotten.lock().unwrap().is_empty());
    }
}
