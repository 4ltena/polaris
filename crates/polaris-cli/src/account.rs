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
static BOUND: OnceLock<Option<BoundTokenSource>> = OnceLock::new();

#[derive(ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 登録の一覧。利用枠の観測はprocessごとのため、別processの取得結果は出ない。
    List,
    /// 新しいアカウントを登録する。ブラウザを開く。
    Login {
        /// 一覧に出す表示名。
        #[arg(long)]
        label: Option<String>,
    },
    /// 既存の登録を同じアカウントで認証し直す。
    Reauth { registration_id: String },
    /// 登録を解除する。実行中のrunは次の送信で止まる。
    Logout { registration_id: String },
    /// 表示名を設定する。
    Label {
        registration_id: String,
        label: String,
    },
}

pub async fn run(args: Args) -> ExitCode {
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
        return Err("HOMEを特定できないためアカウントを扱えません。".into());
    }
    AccountOwner::open(ISSUER).map_err(|error| format!("アカウントを開けません: {error}"))
}

/// 登録が1件でもあるときだけ所有者を返す。
fn registered_owner() -> Result<Option<Arc<AccountOwner>>, String> {
    if trusted_home().is_none() {
        return Ok(None);
    }
    AccountOwner::open_if_registered(ISSUER)
        .map_err(|error| format!("アカウントを開けません: {error}"))
}

fn state_label(state: AccountState) -> &'static str {
    match state {
        AccountState::Registering => "登録中",
        AccountState::Ready => "利用可",
        AccountState::RefreshPending => "更新中",
        AccountState::ReauthRequired => "再認証が必要",
        AccountState::Disconnected => "解除済み",
    }
}

fn list() -> Result<(), String> {
    let Some(owner) = registered_owner()? else {
        println!("登録はありません。単一認証を使用しています。");
        return Ok(());
    };
    let snapshot = owner
        .snapshot()
        .map_err(|error| format!("一覧を取得できません: {error}"))?;
    for account in snapshot.accounts {
        // 観測はこのprocessのメモリだけにあるため、起動直後は常に未取得になる。
        let usage = match owner.observation(&account.registration_id, "gpt-6-astra") {
            None => "利用枠: 未取得".to_string(),
            Some(observed) => format!("利用枠: {}", observed_label(&observed.state)),
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
        ObservationState::Known(_) => "取得済み",
        ObservationState::Missing => "未取得",
        ObservationState::FetchFailed => "取得失敗",
        ObservationState::MappingUnknown => "対応未検証",
        ObservationState::Unsupported => "非対応",
    }
}

/// 予約を取る前に対話端末かどうかを確かめる。非対話ではslotを消費しない。
async fn login(label: Option<String>) -> Result<(), String> {
    if !std::io::stderr().is_terminal() {
        return Err("非対話の端末では登録できません。端末から実行してください。".into());
    }
    let owner = owner()?;
    let token = owner
        .register(label, callback, std::future::pending::<()>())
        .await
        .map_err(|error| format!("登録できません: {error}"))?;
    println!("登録しました: {}", token.registration_id);
    Ok(())
}

async fn reauth(registration_id: &str) -> Result<(), String> {
    if !std::io::stderr().is_terminal() {
        return Err("非対話の端末では再認証できません。端末から実行してください。".into());
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
        .map_err(|error| format!("再認証できません: {error}"))?;
    println!("再認証しました: {registration_id}");
    Ok(())
}

/// 認可URLを開き、折り返しのcodeを1回だけ受け取る。
async fn callback(url: String, state: String) -> Result<String, AuthError> {
    let listener = polaris_auth::login::bind_callback().await?;
    eprintln!("ブラウザで認可してください: {url}");
    let _ = std::process::Command::new(if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    })
    .arg(&url)
    .spawn();
    polaris_auth::login::wait_for_callback(listener, &state, Duration::from_secs(300)).await
}

fn generation_of(owner: &AccountOwner, registration_id: &str) -> Result<u64, String> {
    Ok(owner
        .snapshot()
        .map_err(|error| format!("一覧を取得できません: {error}"))?
        .accounts
        .into_iter()
        .find(|account| account.registration_id == registration_id)
        .ok_or("その登録IDはありません。")?
        .generation)
}

fn logout(registration_id: &str) -> Result<(), String> {
    let owner = owner()?;
    let generation = generation_of(&owner, registration_id)?;
    owner
        .logout(registration_id, generation)
        .map_err(|error| format!("解除できません: {error}"))?;
    println!("解除しました: {registration_id}");
    Ok(())
}

fn set_label(registration_id: &str, label: &str) -> Result<(), String> {
    let owner = owner()?;
    let generation = generation_of(&owner, registration_id)?;
    owner
        .set_label(registration_id, generation, Some(label.to_owned()))
        .map_err(|error| format!("表示名を設定できません: {error}"))?;
    println!("表示名を設定しました: {registration_id}");
    Ok(())
}

/// runの送信先を1つの登録へ固定する。登録が無ければ単一認証のままにする。
pub fn bind(model: &str, explicit: Option<&str>) -> Result<(), String> {
    let Some(owner) = registered_owner()? else {
        if explicit.is_some() {
            return Err(
                "登録済みのアカウントがありません。`polaris account login`で登録してください。"
                    .into(),
            );
        }
        let _ = BOUND.set(None);
        return Ok(());
    };
    // 要約とMCP選定も同じ登録へ送るため、選択の対象に含める。
    let models = [model, "gpt-6-astra", polaris_core::mcp::preselection::MODEL];
    let (registration_id, generation) =
        owner.bind(&models, explicit).map_err(|error| {
            match error {
        AuthError::NotLoggedIn => {
            "使用できる登録がありません。`polaris account list`で状態を確認してください。"
                .to_string()
        }
        AuthError::Denied(reason) if reason == "no_selectable_account" => {
            "利用枠が確認できないため自動選択できません。`--account <登録ID>`で指定してください。"
                .to_string()
        }
        other => format!("アカウントを選べません: {other}"),
    }
        })?;
    let _ = BOUND.set(Some(BoundTokenSource::new(
        owner,
        registration_id,
        generation,
    )));
    Ok(())
}

/// 送信経路のtoken source。登録済みなのに束縛前だと、単一認証へ落ちる代わりに
/// 失敗する。
pub fn require(
    store: std::path::PathBuf,
) -> Result<Arc<dyn polaris_provider::TokenSource>, String> {
    if let Some(bound) = token_source() {
        return Ok(bound);
    }
    if BOUND.get().is_none() && registered_owner()?.is_some() {
        return Err("アカウントを固定する前に送信経路を作れません。".into());
    }
    Ok(Arc::new(crate::AuthTokens {
        issuer: ISSUER.to_string(),
        store,
    }))
}

/// 束縛済みならその登録だけを使うtoken source。未束縛なら単一認証へ任せる。
fn token_source() -> Option<Arc<dyn polaris_provider::TokenSource>> {
    BOUND.get().and_then(Option::as_ref).map(|bound| {
        Arc::new(Tokens(bound.clone(), std::sync::Mutex::new(None)))
            as Arc<dyn polaris_provider::TokenSource>
    })
}

struct Tokens(BoundTokenSource, std::sync::Mutex<Option<u64>>);

#[async_trait::async_trait]
impl polaris_provider::TokenSource for Tokens {
    async fn token(&self) -> Result<polaris_provider::Token, polaris_provider::ProviderError> {
        self.convert(self.0.token().await)
    }

    async fn refreshed(&self) -> Result<polaris_provider::Token, polaris_provider::ProviderError> {
        // 401のときだけ呼ばれる。使った版を所有者へ渡し、再取得は所有者が決める。
        let used = *self.1.lock().unwrap_or_else(|e| e.into_inner());
        let token = match used {
            Some(revision) => self.0.refresh_after_unauthorized(revision).await,
            None => self.0.token().await,
        };
        self.convert(token)
    }
}

impl Tokens {
    fn convert(
        &self,
        token: Result<polaris_auth::accounts::owner::AccessToken, AuthError>,
    ) -> Result<polaris_provider::Token, polaris_provider::ProviderError> {
        let token = token.map_err(crate::to_provider_error)?;
        *self.1.lock().unwrap_or_else(|e| e.into_inner()) = Some(token.token_revision);
        Ok(polaris_provider::Token {
            effort: crate::effort_for(token.secret()),
            access_token: token.secret().to_owned(),
            account_id: token.account_id.clone(),
        })
    }
}
