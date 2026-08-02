//! Shared test-only helpers.
//!
//! Single authoritative home for the `Config` builders, temp-dir `Database`
//! constructor, and DB row seeding that the HTTP/auth test modules previously
//! each carried a byte-identical copy of. Compiled only under `cfg(test)`
//! (see the `mod test_support` declaration in `main.rs`).

use std::path::PathBuf;
use std::sync::Arc;

/// Minimal `Config` for tests (token sets whether auth is enabled).
pub(crate) fn test_config(token: Option<&str>) -> Arc<crate::Config> {
    Arc::new(crate::Config {
        addr: "127.0.0.1:0".to_string(),
        data_dir: PathBuf::from("./data"),
        token: token.map(str::to_string),
        max_text_size: 2 * 1024 * 1024,
        max_file_size: 100 * 1024 * 1024,
        codex: crate::CodexConfig::default(),
        oauth2: crate::OAuth2Config::default(),
    })
}

/// Like [`test_config`] but with OAuth2 enabled (1h access-token TTL,
/// 30d refresh-token TTL).
pub(crate) fn test_config_oauth2(token: Option<&str>) -> Arc<crate::Config> {
    Arc::new(crate::Config {
        addr: "127.0.0.1:0".to_string(),
        data_dir: PathBuf::from("./data"),
        token: token.map(str::to_string),
        max_text_size: 2 * 1024 * 1024,
        max_file_size: 100 * 1024 * 1024,
        codex: crate::CodexConfig::default(),
        oauth2: crate::OAuth2Config {
            enabled: true,
            access_token_ttl_secs: 3600,
            refresh_token_ttl_secs: 2_592_000,
            ..crate::OAuth2Config::default()
        },
    })
}

/// Create an empty Database in a temp dir. The TempDir must be kept alive
/// for the lifetime of the returned Database so the sqlite file is not
/// deleted mid-test.
pub(crate) fn test_db() -> (tempfile::TempDir, Arc<crate::Database>) {
    let tmp = tempfile::tempdir().unwrap();
    let db = crate::Database::open(&tmp.path().join("test.db")).unwrap();
    (tmp, Arc::new(db))
}

/// Bootstrap helper: create a user with the given role directly via the DB
/// so tests can mint tokens for them.
pub(crate) fn seed_user_with_role(
    db: &crate::Database,
    username: &str,
    role: &str,
) -> crate::models::UserRecord {
    let now = chrono::Utc::now().timestamp();
    let user = crate::models::UserRecord {
        id: uuid::Uuid::new_v4().to_string(),
        username: username.to_string(),
        created_at: now,
        disabled: 0,
        display_name: None,
        role: role.to_string(),
        disabled_at: None,
        updated_at: Some(now),
    };
    db.create_user(&user).unwrap();
    user
}

/// [`seed_user_with_role`] with the default `"user"` role.
pub(crate) fn seed_user(db: &crate::Database, username: &str) -> crate::models::UserRecord {
    seed_user_with_role(db, username, "user")
}

/// Shared body for the two `seed_oauth_client*` shapes below.
fn seed_oauth_client_record(
    db: &crate::Database,
    user: &crate::models::UserRecord,
    name: &str,
    allowed_scopes: &str,
) -> (crate::models::OAuthClientRecord, String) {
    let now = chrono::Utc::now().timestamp();
    let plaintext_secret = crate::auth::generate_oauth_client_secret();
    let record = crate::models::OAuthClientRecord {
        id: uuid::Uuid::new_v4().to_string(),
        client_id: crate::auth::generate_oauth_client_id(),
        client_secret_hash: crate::auth::hash_token(&plaintext_secret),
        name: name.to_string(),
        owner_user_id: user.id.clone(),
        redirect_uris: "https://example.com/callback".to_string(),
        allowed_scopes: allowed_scopes.to_string(),
        created_at: now,
        revoked_at: None,
    };
    db.insert_oauth_client(&record).unwrap();
    (record, plaintext_secret)
}

/// Seed an OAuth2 client ("Test App") owned by `user` with the broad scope
/// set used by the mcp/runtime_http HTTP tests. The plaintext secret is
/// discarded.
pub(crate) fn seed_oauth_client(
    db: &crate::Database,
    user: &crate::models::UserRecord,
) -> crate::models::OAuthClientRecord {
    seed_oauth_client_record(
        db,
        user,
        "Test App",
        "runtime:read project:read project:write job:run account:manage",
    )
    .0
}

/// Seed a named OAuth2 client with the narrow `runtime:read project:read`
/// scope set and return `(record, plaintext_secret)`.
pub(crate) fn seed_oauth_client_named(
    db: &crate::Database,
    user: &crate::models::UserRecord,
    name: &str,
) -> (crate::models::OAuthClientRecord, String) {
    seed_oauth_client_record(db, user, name, "runtime:read project:read")
}

/// Single source of truth for the bounded `show_changes` framing wire format.
///
/// This is the only place a `WCSF1` trailer is constructed for tests. The
/// trailer carries the exact wire byte lengths of the body and metadata so the
/// production parser can walk backward over them without scanning for the
/// legacy delimiter; hand-writing a second copy of this format is exactly what
/// this helper exists to prevent. All fixture content is ASCII, so
/// `as_bytes().len()` matches both the production byte lengths and the
/// byte-based walk-back in `parse_show_changes_wire_block`.
///
/// The layout mirrors the production command exactly: the metadata region is
/// emitted with a trailing `\n` (the production `printf '%s\n' "$sm"`) and the
/// declared metadata byte count includes it, because the parser's
/// `strip_wire_lf` requires a trailing newline before it will accept a frame.
/// `body` is passed through verbatim as the wire body.
pub(crate) fn framed_show_changes_block(kind: char, body: &str, metadata: &str) -> String {
    format!(
        "{body}{metadata}\nWCSF1:{kind}:{:010}:{:010}\n",
        body.as_bytes().len(),
        metadata.as_bytes().len() + 1,
    )
}

/// Build a valid production `show_changes` stdout payload for an
/// `include_diff=false` run.
///
/// The three frames (status `S`, head `H`, diff-stat `T`) are emitted in wire
/// order and their metadata is derived from the given bodies, so the result is
/// genuinely `transport_safe` rather than merely frame-decodable: per-category
/// counts and `files_*` come from the status body, and every frame's byte
/// metadata matches its body. `files_limit` mirrors the production constant
/// `SHOW_CHANGES_MAX_STATUS_FILES` (200).
///
/// `status_body` must end in `\n` (as the production streaming loop always
/// emits, so the parser can strip its wire newline); `head_body` is emitted
/// without a trailing newline to match the production `printf '%s%s'` head
/// frame.
pub(crate) fn framed_show_changes_stdout(
    status_body: &str,
    head_body: &str,
    stat_body: &str,
) -> String {
    let status = status_body.strip_suffix('\n').unwrap_or(status_body);
    let mut records = 0usize;
    let mut modified = 0usize;
    let mut added = 0usize;
    let mut deleted = 0usize;
    let mut renamed = 0usize;
    let mut copied = 0usize;
    let mut untracked = 0usize;
    let mut conflicted = 0usize;
    let mut staged = 0usize;
    let mut unstaged = 0usize;
    for line in status.lines().filter(|line| !line.starts_with("## ")) {
        if line.len() < 3 {
            continue;
        }
        let mut chars = line.chars();
        let x = chars.next().unwrap_or(' ');
        let y = chars.next().unwrap_or(' ');
        records += 1;
        if x == '?' && y == '?' {
            untracked += 1;
        } else if x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D') {
            conflicted += 1;
        } else if x == 'R' || y == 'R' {
            renamed += 1;
        } else if x == 'C' || y == 'C' {
            copied += 1;
        } else if x == 'D' || y == 'D' {
            deleted += 1;
        } else if x == 'A' || y == 'A' {
            added += 1;
        } else {
            modified += 1;
        }
        if !(x == '?' && y == '?')
            && !(x == 'U' || y == 'U' || (x == 'A' && y == 'A') || (x == 'D' && y == 'D'))
        {
            if x != ' ' && x != '?' {
                staged += 1;
            }
            if y != ' ' && y != '?' {
                unstaged += 1;
            }
        }
    }
    let status_meta = format!(
        "status_exit=0\nrepository_probe=inside_worktree\nrepository_probe_exit=0\n\
         files_total={records}\nfiles_returned={records}\nfiles_truncated=0\nfiles_limit=200\n\
         status_bytes={}\nstatus_trunc_count=0\nstatus_trunc_bytes=0\nstatus_trunc_path=0\n\
         modified={modified}\nadded={added}\ndeleted={deleted}\nrenamed={renamed}\n\
         copied={copied}\nuntracked={untracked}\nconflicted={conflicted}\n\
         staged={staged}\nunstaged={unstaged}",
        status.as_bytes().len(),
    );
    // The head frame body has no trailing newline (production `printf '%s%s'`);
    // the head metadata's `head_bytes` counts exactly that wire body so the
    // `frame_bytes_match` validation holds.
    let head = head_body.strip_suffix('\n').unwrap_or(head_body);
    let head_meta = format!("head_exit=0\nhead_truncated=0\nhead_bytes={}", head.len());
    let stat = stat_body.strip_suffix('\n').unwrap_or(stat_body);
    let stat_meta = format!(
        "diff_stat_exit=0\ndiff_stat_truncated=0\ndiff_stat_bytes={}",
        stat.as_bytes().len()
    );
    format!(
        "{}{}{}",
        framed_show_changes_block('S', status_body, &status_meta),
        framed_show_changes_block('H', head, &head_meta),
        framed_show_changes_block('T', stat, &stat_meta),
    )
}
