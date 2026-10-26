//! 複数アカウントの登録操作と、1つのrunを1つの登録へ固定する束縛。
//!
//! 登録が1件も無い間は従来の単一認証のままで、登録が1件でもあれば単一認証へ
//! 戻らない。観測はprocessのメモリにしか無いため、一覧は取得済みかどうかを
//! そのまま示す。
use clap::{Args as ClapArgs, Subcommand};
use polaris_auth::accounts::observation::BoundTokenSource;
use polaris_auth::accounts::owner::AccountOwner;
use polaris_auth::accounts::store::AccountState;
use polaris_auth::{AuthError, ISSUER};
use std::io::IsTerminal;
use std::process::ExitCode;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// このprocessのrunが使う登録。`None`は単一認証のまま。
static BOUND: OnceLock<Arc<Binding>> = OnceLock::new();

pub struct Binding {
    current: std::sync::RwLock<Option<BoundTokenSource>>,
}

#[cfg(unix)]
impl polaris_tui::account::BindingControl for Binding {
    fn current(&self) -> Option<(String, u64)> {
        self.current
            .read()
            .ok()?
            .as_ref()
            .map(|bound| (bound.registration_id().to_owned(), bound.generation()))
    }

    fn bind(&self, registration: &str) -> Result<String, String> {
        let owner = owner()?;
        let (id, generation) = owner
            .bind(&[], Some(registration))
            .map_err(|_| "Cannot switch to that registration".to_owned())?;
        *self
            .current
            .write()
            .map_err(|_| "Cannot update the account binding")? =
            Some(BoundTokenSource::new(owner, id.clone(), generation));
        Ok(id)
    }
}

#[cfg(unix)]
pub fn binding_control() -> Option<Arc<dyn polaris_tui::account::BindingControl>> {
    BOUND
        .get()
        .map(|bound| bound.clone() as Arc<dyn polaris_tui::account::BindingControl>)
}

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List registrations. Usage observations belong to this process; observations from other processes are not shown.
    List,
    /// Register a new account using the browser.
    Login {
        /// Display name in the account list.
        #[arg(long)]
        label: Option<String>,
    },
    /// Reauthenticate an existing registration with the same account.
    Reauth { registration_id: String },
    /// Unlink a registration. Active runs stop at their next send.
    Logout { registration_id: String },
    /// Set the display name.
    Label {
        registration_id: String,
        label: String,
    },
}

pub async fn run(args: Args) -> ExitCode {
    if cfg!(windows) {
        eprintln!("Protected account storage is unavailable on Windows. No changes were made.");
        return ExitCode::FAILURE;
    }
    let result = match args.command {
        Command::List => list(),
        Command::Login { label } => login(label).await,
        Command::Reauth { registration_id } => reauth(&registration_id).await,
        Command::Logout { registration_id } => logout(&registration_id),
        Command::Label {
            registration_id,
            label,
        } => set_label(&registration_id, &label),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// accountsを読む前にこのprocessのhomeを固定する。固定できない環境では登録も
/// 作れないため、単一認証のままとして扱う。
fn trusted_home() -> Option<std::path::PathBuf> {
    let home = std::path::PathBuf::from(std::env::var_os("HOME")?);
    polaris_auth::protection::use_trusted_home(&home)
        .is_ok()
        .then_some(home)
}

fn owner() -> Result<Arc<AccountOwner>, String> {
    if trusted_home().is_none() {
        return Err("Cannot manage accounts because HOME could not be determined.".into());
    }
    AccountOwner::open(ISSUER).map_err(|error| format!("Cannot open accounts: {error}"))
}

/// 登録が1件でもあるときだけ所有者を返す。
fn registered_owner() -> Result<Option<Arc<AccountOwner>>, String> {
    if trusted_home().is_none() {
        return Ok(None);
    }
    AccountOwner::open_if_registered(ISSUER)
        .map_err(|error| format!("Cannot open accounts: {error}"))
}

pub(super) enum StartupAccounts {
    Legacy,
    LoggedOut(Arc<AccountOwner>),
    Registered,
}

fn all_disconnected(states: &[AccountState]) -> bool {
    !states.is_empty()
        && states
            .iter()
            .all(|state| *state == AccountState::Disconnected)
}

pub(super) fn has_registrations() -> Result<bool, String> {
    registered_owner().map(|owner| owner.is_some())
}

/// 解除済みの登録だけを新しいログインへ案内する。再認証や処理中は混同しない。
pub(super) fn startup() -> Result<StartupAccounts, String> {
    let Some(owner) = registered_owner()? else {
        return Ok(StartupAccounts::Legacy);
    };
    let snapshot = owner
        .snapshot()
        .map_err(|error| format!("Cannot retrieve the list: {error}"))?;
    let states: Vec<_> = snapshot
        .accounts
        .iter()
        .map(|account| account.state)
        .collect();
    Ok(if all_disconnected(&states) {
        StartupAccounts::LoggedOut(owner)
    } else {
        StartupAccounts::Registered
    })
}

fn state_label(state: AccountState) -> &'static str {
    match state {
        AccountState::Registering => "registering",
        AccountState::Ready => "available",
        AccountState::RefreshPending => "updating",
        AccountState::ReauthRequired => "reauthentication required",
        AccountState::Disconnected => "unlinked",
    }
}

fn list() -> Result<(), String> {
    let Some(owner) = registered_owner()? else {
        println!("No registrations. Using legacy single-account authentication.");
        return Ok(());
    };
    let snapshot = owner
        .snapshot()
        .map_err(|error| format!("Cannot retrieve the list: {error}"))?;
    for account in snapshot.accounts {
        // 観測はこのprocessのメモリだけにあるため、起動直後は常に未取得になる。
        let usage = match owner.observation(&account.registration_id, "gpt-6-astra") {
            None => "Usage limits: not retrieved".to_string(),
            Some(observed) => format!("Usage limits: {}", observed_label(&observed.state)),
        };
        println!(
            "{}  {}  {}  {}",
            account.registration_id,
            state_label(account.state),
            account.label.as_deref().unwrap_or("-"),
            usage
        );
    }
    Ok(())
}

fn observed_label(state: &polaris_auth::quota::ObservationState) -> &'static str {
    use polaris_auth::quota::ObservationState;
    match state {
        ObservationState::Known(_) => "retrieved",
        ObservationState::Missing => "not retrieved",
        ObservationState::FetchFailed => "retrieval failed",
        ObservationState::MappingUnknown => "support unverified",
        ObservationState::Unsupported => "unsupported",
    }
}

/// 予約を取る前に対話端末かどうかを確かめる。非対話ではslotを消費しない。
async fn login(label: Option<String>) -> Result<(), String> {
    if !std::io::stderr().is_terminal() {
        return Err(
            "Registration requires an interactive terminal. Run this command in a terminal.".into(),
        );
    }
    let owner = owner()?;
    let token = owner
        .register(label, callback, std::future::pending::<()>())
        .await
        .map_err(|error| format!("Cannot register: {error}"))?;
    println!("Registered: {}", token.registration_id);
    Ok(())
}

async fn reauth(registration_id: &str) -> Result<(), String> {
    if !std::io::stderr().is_terminal() {
        return Err(
            "Reauthentication requires an interactive terminal. Run this command in a terminal."
                .into(),
        );
    }
    let owner = owner()?;
    let generation = generation_of(&owner, registration_id)?;
    owner
        .reauthenticate(
            registration_id,
            generation,
            callback,
            std::future::pending::<()>(),
        )
        .await
        .map_err(|error| format!("Cannot reauthenticate: {error}"))?;
    println!("Reauthenticated: {registration_id}");
    Ok(())
}

/// 認可URLを開き、折り返しのcodeを1回だけ受け取る。
async fn callback(url: String, state: String) -> Result<String, AuthError> {
    let listener = polaris_auth::login::bind_callback().await?;
    eprintln!("Authorize in your browser: {url}");
    if polaris_auth::login::launch_browser(&url).await.is_err() {
        eprintln!(
            "Could not open the browser. Open the link above manually; sign-in is still waiting."
        );
    }
    polaris_auth::login::wait_for_callback(listener, &state, Duration::from_secs(300)).await
}

fn generation_of(owner: &AccountOwner, registration_id: &str) -> Result<u64, String> {
    Ok(owner
        .snapshot()
        .map_err(|error| format!("Cannot retrieve the list: {error}"))?
        .accounts
        .into_iter()
        .find(|account| account.registration_id == registration_id)
        .ok_or("Registration ID not found.")?
        .generation)
}

fn logout(registration_id: &str) -> Result<(), String> {
    let owner = owner()?;
    let generation = generation_of(&owner, registration_id)?;
    owner
        .logout(registration_id, generation)
        .map_err(|error| format!("Cannot unlink: {error}"))?;
    println!("Unlinked: {registration_id}");
    Ok(())
}

fn set_label(registration_id: &str, label: &str) -> Result<(), String> {
    let owner = owner()?;
    let generation = generation_of(&owner, registration_id)?;
    owner
        .set_label(registration_id, generation, Some(label.to_owned()))
        .map_err(|error| format!("Cannot set the display name: {error}"))?;
    println!("Display name set: {registration_id}");
    Ok(())
}

/// runの送信先を1つの登録へ固定する。登録が無ければ単一認証のままにする。
pub fn bind(model: &str, explicit: Option<&str>) -> Result<(), String> {
    let Some(owner) = registered_owner()? else {
        if explicit.is_some() {
            return Err("No registered accounts. Register with `polaris account login`.".into());
        }
        let _ = BOUND.set(Arc::new(Binding {
            current: std::sync::RwLock::new(None),
        }));
        return Ok(());
    };
    // 要約とMCP選定も同じ登録へ送るため、選択の対象に含める。
    let models = [model, "gpt-6-astra", polaris_core::mcp::preselection::MODEL];
    let (registration_id, generation) =
        owner.bind(&models, explicit).map_err(|error| {
            match error {
        AuthError::NotLoggedIn => {
            match owner.snapshot() {
                Ok(snapshot) => {
                    let reauth: Vec<_> = snapshot.accounts.iter()
                        .filter(|account| account.state == AccountState::ReauthRequired)
                        .map(|account| format!("polaris account reauth {}", account.registration_id))
                        .collect();
                    if reauth.is_empty() {
                        "No usable registrations. Check their status with `polaris account list`.".into()
                    } else {
                        format!("Reauthentication required. Run this command in a terminal:\n{}", reauth.join("\n"))
                    }
                }
                Err(error) => format!("Cannot inspect accounts: {error}"),
            }
        }
        AuthError::Denied(reason) if reason == "no_selectable_account" => {
            "Cannot select automatically because usage limits are unknown. Specify `--account <registration-id>`."
                .to_string()
        }
        other => format!("Cannot select an account: {other}"),
    }
        })?;
    let _ = BOUND.set(Arc::new(Binding {
        current: std::sync::RwLock::new(Some(BoundTokenSource::new(
            owner,
            registration_id,
            generation,
        ))),
    }));
    Ok(())
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    #[test]
    fn only_disconnected_registrations_reenter_login() {
        assert!(all_disconnected(&[AccountState::Disconnected]));
        assert!(all_disconnected(&[
            AccountState::Disconnected,
            AccountState::Disconnected
        ]));
        assert!(!all_disconnected(&[]));
        for state in [
            AccountState::Ready,
            AccountState::Registering,
            AccountState::RefreshPending,
            AccountState::ReauthRequired,
        ] {
            assert!(!all_disconnected(&[state]));
            assert!(!all_disconnected(&[AccountState::Disconnected, state]));
        }
    }
}

/// 送信経路のtoken source。登録済みなのに束縛前だと、単一認証へ落ちる代わりに
/// 失敗する。
pub fn require(
    store: std::path::PathBuf,
) -> Result<Arc<dyn polaris_provider::TokenSource>, String> {
    let Some(binding) = BOUND.get() else {
        if registered_owner()?.is_some() {
            return Err("Cannot create a transport before binding an account.".into());
        }
        return Ok(Arc::new(crate::AuthTokens {
            issuer: ISSUER.to_string(),
            store,
        }));
    };
    Ok(Arc::new(Tokens {
        binding: binding.clone(),
        legacy: crate::AuthTokens {
            issuer: ISSUER.to_string(),
            store,
        },
        used: std::sync::Mutex::new(None),
    }))
}

struct Tokens {
    binding: Arc<Binding>,
    legacy: crate::AuthTokens,
    // A 401 belongs to the exact registration/generation used by that request.
    used: std::sync::Mutex<Option<(String, u64, u64)>>,
}

impl Tokens {
    fn bound(&self) -> Result<Option<BoundTokenSource>, polaris_provider::ProviderError> {
        let current = self
            .binding
            .current
            .read()
            .map_err(|_| crate::to_provider_error(AuthError::Protection))?
            .clone();
        if current.is_none()
            && registered_owner()
                .map_err(|_| crate::to_provider_error(AuthError::Protection))?
                .is_some()
        {
            return Err(crate::to_provider_error(AuthError::Denied(
                "explicit_account_binding_required".into(),
            )));
        }
        Ok(current)
    }

    fn convert(
        &self,
        token: Result<polaris_auth::accounts::owner::AccessToken, AuthError>,
    ) -> Result<polaris_provider::Token, polaris_provider::ProviderError> {
        let token = token.map_err(crate::to_provider_error)?;
        *self.used.lock().unwrap_or_else(|e| e.into_inner()) = Some((
            token.registration_id.clone(),
            token.generation,
            token.token_revision,
        ));
        Ok(polaris_provider::Token {
            effort: crate::effort_for(token.secret()),
            access_token: token.secret().to_owned(),
            account_id: token.account_id.clone(),
        })
    }
}

#[async_trait::async_trait]
impl polaris_provider::TokenSource for Tokens {
    async fn token(&self) -> Result<polaris_provider::Token, polaris_provider::ProviderError> {
        match self.bound()? {
            Some(bound) => self.convert(bound.token().await),
            None => polaris_provider::TokenSource::token(&self.legacy).await,
        }
    }

    async fn refreshed(&self) -> Result<polaris_provider::Token, polaris_provider::ProviderError> {
        let Some(bound) = self.bound()? else {
            return polaris_provider::TokenSource::refreshed(&self.legacy).await;
        };
        let used = self.used.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let result = match used {
            Some((id, generation, revision))
                if id == bound.registration_id() && generation == bound.generation() =>
            {
                bound.refresh_after_unauthorized(revision).await
            }
            Some(_) => {
                return Err(crate::to_provider_error(AuthError::Denied(
                    "binding_changed_during_request".into(),
                )));
            }
            None => bound.token().await,
        };
        self.convert(result)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use polaris_tui::account::BindingControl;

    #[tokio::test]
    async fn m3a_shared_binding_is_explicit_and_rejects_a_foreign_refresh() {
        if std::env::var_os("POLARIS_M3A_BINDING_CHILD").is_none() {
            let home = tempfile::tempdir().unwrap();
            std::fs::create_dir(home.path().join("tmp")).unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "account::tests::m3a_shared_binding_is_explicit_and_rejects_a_foreign_refresh",
                    "--exact",
                    "--nocapture",
                ])
                .env("POLARIS_M3A_BINDING_CHILD", "1")
                .env("HOME", home.path())
                .env("TMPDIR", home.path().join("tmp"))
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        polaris_auth::protection::use_trusted_home(&home).unwrap();
        let store = polaris_auth::accounts::store::AccountStore::open().unwrap();
        let mut ids = Vec::new();
        for principal in ["fixture-a", "fixture-b"] {
            let reservation = store.begin_registration(None).unwrap();
            store.mark_exchange_pending(&reservation).unwrap();
            store
                .complete_registration(
                    &reservation,
                    polaris_auth::Credentials {
                        access_token: format!("synthetic-{principal}"),
                        refresh_token: format!("synthetic-refresh-{principal}"),
                        account_id: principal.into(),
                        expires_at: Some(u64::MAX),
                    },
                )
                .unwrap();
            ids.push(reservation.registration_id);
        }
        bind("gpt-6-astra", Some(&ids[0])).unwrap();
        let tokens = require(polaris_auth::store::default_path().unwrap()).unwrap();
        assert_eq!(tokens.token().await.unwrap().account_id, "fixture-a");
        let binding = BOUND.get().unwrap();
        assert_eq!(binding.current().unwrap().0, ids[0]);
        assert!(binding.bind("missing-registration").is_err());
        assert_eq!(binding.current().unwrap().0, ids[0]);
        binding.bind(&ids[1]).unwrap();
        assert!(tokens.refreshed().await.is_err());
        assert_eq!(tokens.token().await.unwrap().account_id, "fixture-b");
        let summary = store
            .snapshot()
            .unwrap()
            .accounts
            .into_iter()
            .find(|a| a.registration_id == ids[1])
            .unwrap();
        store
            .logout(&summary.registration_id, summary.generation)
            .unwrap();
        assert!(
            tokens.token().await.is_err(),
            "removed binding must never fall back to the other account"
        );
        assert_eq!(binding.current().unwrap().0, ids[1]);
    }
}
