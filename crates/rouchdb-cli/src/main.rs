use std::io::{self, BufWriter, Write};
use std::process;

use clap::{Parser, Subcommand};
use rouchdb::{
    AllDocsOptions, ChangesOptions, Database, FindOptions, GetOptions, ReplicationOptions,
};

#[derive(Parser)]
#[command(name = "rouchdb", about = "Inspect and query RouchDB redb databases")]
struct Cli {
    /// Pretty-print JSON output
    #[arg(short, long, global = true)]
    pretty: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show database info (doc count, update sequence)
    Info {
        /// Path to the .redb file
        path: String,
        /// Database name (defaults to filename without extension)
        #[arg(long)]
        db_name: Option<String>,
    },

    /// Get a single document by ID
    Get {
        /// Path to the .redb file
        path: String,
        /// Document ID
        doc_id: String,
        /// Fetch a specific revision
        #[arg(long)]
        rev: Option<String>,
        /// Include conflict information
        #[arg(long)]
        conflicts: bool,
        /// Database name (defaults to filename without extension)
        #[arg(long)]
        db_name: Option<String>,
    },

    /// List all documents
    AllDocs {
        /// Path to the .redb file
        path: String,
        /// Include full document bodies
        #[arg(long)]
        include_docs: bool,
        /// Start key for range query
        #[arg(long)]
        start_key: Option<String>,
        /// End key for range query
        #[arg(long)]
        end_key: Option<String>,
        /// Maximum number of documents to return
        #[arg(long)]
        limit: Option<u64>,
        /// Number of documents to skip
        #[arg(long, default_value = "0")]
        skip: u64,
        /// Reverse the order of results
        #[arg(long)]
        descending: bool,
        /// Database name (defaults to filename without extension)
        #[arg(long)]
        db_name: Option<String>,
    },

    /// Query documents using a Mango selector
    Find {
        /// Path to the .redb file
        path: String,
        /// Mango selector as JSON string
        #[arg(long)]
        selector: String,
        /// Comma-separated list of fields to return
        #[arg(long)]
        fields: Option<String>,
        /// Sort specification as JSON (e.g. '[{"age": "asc"}]')
        #[arg(long)]
        sort: Option<String>,
        /// Maximum number of results
        #[arg(long)]
        limit: Option<u64>,
        /// Number of results to skip
        #[arg(long)]
        skip: Option<u64>,
        /// Database name (defaults to filename without extension)
        #[arg(long)]
        db_name: Option<String>,
    },

    /// Show the changes feed
    Changes {
        /// Path to the .redb file
        path: String,
        /// Start after this sequence number
        #[arg(long, default_value = "0")]
        since: u64,
        /// Maximum number of changes
        #[arg(long)]
        limit: Option<u64>,
        /// Include full document bodies
        #[arg(long)]
        include_docs: bool,
        /// Reverse the order
        #[arg(long)]
        descending: bool,
        /// Database name (defaults to filename without extension)
        #[arg(long)]
        db_name: Option<String>,
    },

    /// Export all documents as a JSON array
    Dump {
        /// Path to the .redb file
        path: String,
        /// Database name (defaults to filename without extension)
        #[arg(long)]
        db_name: Option<String>,
    },

    /// Replicate between a redb file and CouchDB (or two redb files)
    ///
    /// CouchDB credentials can be embedded in the URL
    /// (http://user:pass@host:5984/db) or, to keep the password out of shell
    /// history and process listings, read from the ROUCHDB_USER and
    /// ROUCHDB_PASSWORD environment variables. The variables apply to every
    /// http(s) source or target whose URL has no credentials of its own.
    Replicate {
        /// Source: path to an existing .redb file or CouchDB URL
        source: String,
        /// Target: path to .redb file or CouchDB URL
        target: String,
        /// Mango selector to filter documents (JSON string)
        #[arg(long)]
        selector: Option<String>,
        /// Database name for source (if redb)
        #[arg(long)]
        source_name: Option<String>,
        /// Database name for target (if redb)
        #[arg(long)]
        target_name: Option<String>,
    },

    /// Compact the database
    Compact {
        /// Path to the .redb file
        path: String,
        /// Database name (defaults to filename without extension)
        #[arg(long)]
        db_name: Option<String>,
    },

    /// Create or update a document
    Put {
        /// Path to the .redb file
        path: String,
        /// Document ID
        doc_id: String,
        /// Document body as JSON string
        body: String,
        /// Current revision (required for updates)
        #[arg(long)]
        rev: Option<String>,
        /// Auto-fetch current revision before updating (upsert)
        #[arg(long, short)]
        force: bool,
        /// Database name (defaults to filename without extension)
        #[arg(long)]
        db_name: Option<String>,
    },

    /// Delete a document
    Delete {
        /// Path to the .redb file
        path: String,
        /// Document ID
        doc_id: String,
        /// Current revision (required)
        #[arg(long)]
        rev: String,
        /// Database name (defaults to filename without extension)
        #[arg(long)]
        db_name: Option<String>,
    },

    /// Create a document with an auto-generated ID
    Post {
        /// Path to the .redb file
        path: String,
        /// Document body as JSON string
        body: String,
        /// Database name (defaults to filename without extension)
        #[arg(long)]
        db_name: Option<String>,
    },

    /// Import documents from a JSON file (array of objects)
    Import {
        /// Path to the .redb file
        path: String,
        /// Path to the JSON file to import (array of docs, each with "_id")
        file: String,
        /// Database name (defaults to filename without extension)
        #[arg(long)]
        db_name: Option<String>,
    },
}

fn infer_db_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rouchdb")
        .to_string()
}

fn open_db(path: &str, name: Option<&str>) -> Database {
    let db_name = name
        .map(String::from)
        .unwrap_or_else(|| infer_db_name(path));
    match Database::open(path, &db_name) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("Error opening database: {}", e);
            process::exit(1);
        }
    }
}

/// Open a database file that must already exist. Commands that only read (or
/// modify existing docs) use this so a mistyped path fails instead of
/// silently creating an empty database.
fn open_existing_db(path: &str, name: Option<&str>) -> Database {
    if !std::path::Path::new(path).exists() {
        eprintln!("Error: database file '{}' does not exist", path);
        process::exit(1);
    }
    open_db(path, name)
}

fn open_source_or_target(path_or_url: &str, name: Option<&str>, must_exist: bool) -> Database {
    if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
        Database::http(&with_env_credentials(path_or_url))
    } else if must_exist {
        open_existing_db(path_or_url, name)
    } else {
        open_db(path_or_url, name)
    }
}

/// Environment variables holding CouchDB credentials, so they do not have to
/// be written into the URL (and end up in shell history or `ps`).
const USER_ENV: &str = "ROUCHDB_USER";
const PASSWORD_ENV: &str = "ROUCHDB_PASSWORD";

/// Add `ROUCHDB_USER` / `ROUCHDB_PASSWORD` to the URL's userinfo, unless the
/// URL already carries credentials or no user is set. reqwest turns the
/// userinfo into a Basic auth header and strips it from the request URL.
fn with_env_credentials(url: &str) -> String {
    let user = match std::env::var(USER_ENV) {
        Ok(user) if !user.is_empty() => user,
        _ => return url.to_string(),
    };
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if rest[..authority_end].contains('@') {
        return url.to_string();
    }

    let mut userinfo = percent_encode_userinfo(&user);
    if let Ok(password) = std::env::var(PASSWORD_ENV) {
        userinfo.push(':');
        userinfo.push_str(&percent_encode_userinfo(&password));
    }
    format!("{}://{}@{}", scheme, userinfo, rest)
}

fn percent_encode_userinfo(s: &str) -> String {
    let mut encoded = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{:02X}", b));
        }
    }
    encoded
}

/// Mask the password of every `scheme://user:password@host` URL in `text`.
/// reqwest normally strips credentials from the URL it puts in its errors,
/// but keeps them when it cannot percent-decode the userinfo.
fn redact_credentials(text: &str) -> String {
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find("://") {
        let (head, tail) = rest.split_at(idx + 3);
        redacted.push_str(head);
        let authority_end = tail
            .find(|c: char| matches!(c, '/' | '?' | '#') || c.is_whitespace())
            .unwrap_or(tail.len());
        match tail[..authority_end].rfind('@') {
            Some(at) => {
                let userinfo = &tail[..at];
                match userinfo.split_once(':') {
                    Some((user, _)) => {
                        redacted.push_str(user);
                        redacted.push_str(":***");
                    }
                    None => redacted.push_str(userinfo),
                }
                rest = &tail[at..];
            }
            None => rest = tail,
        }
    }
    redacted.push_str(rest);
    redacted
}

fn check_doc_result(result: &rouchdb::DocResult) -> rouchdb::Result<()> {
    if !result.ok {
        let reason = result
            .reason
            .as_deref()
            .or(result.error.as_deref())
            .unwrap_or("document update conflict");
        return Err(rouchdb::RouchError::BadRequest(format!(
            "{}: {}",
            result.id, reason
        )));
    }
    Ok(())
}

fn print_json(value: &serde_json::Value, pretty: bool) {
    let mut out = BufWriter::new(io::stdout().lock());
    let result = if pretty {
        serde_json::to_writer_pretty(&mut out, value)
    } else {
        serde_json::to_writer(&mut out, value)
    }
    .map_err(io::Error::from)
    .and_then(|()| writeln!(out))
    .and_then(|()| out.flush());

    match result {
        Ok(()) => {}
        // The reader went away (e.g. `rouchdb dump db.redb | head`): stop
        // writing and let the command finish instead of panicking.
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {}
        Err(e) => {
            eprintln!("Error writing output: {}", e);
            process::exit(1);
        }
    }
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = run(cli).await;
    if let Err(e) = result {
        eprintln!("Error: {}", redact_credentials(&e.to_string()));
        process::exit(1);
    }
}

async fn run(cli: Cli) -> rouchdb::Result<()> {
    match cli.command {
        Commands::Info { path, db_name } => {
            let db = open_existing_db(&path, db_name.as_deref());
            let info = db.info().await?;
            print_json(&serde_json::to_value(&info).unwrap(), cli.pretty);
        }

        Commands::Get {
            path,
            doc_id,
            rev,
            conflicts,
            db_name,
        } => {
            let db = open_existing_db(&path, db_name.as_deref());
            let doc = db
                .get_with_opts(
                    &doc_id,
                    GetOptions {
                        rev,
                        conflicts,
                        ..Default::default()
                    },
                )
                .await?;
            print_json(&doc.to_json(), cli.pretty);
        }

        Commands::AllDocs {
            path,
            include_docs,
            start_key,
            end_key,
            limit,
            skip,
            descending,
            db_name,
        } => {
            let db = open_existing_db(&path, db_name.as_deref());
            let response = db
                .all_docs(AllDocsOptions {
                    include_docs,
                    start_key,
                    end_key,
                    limit,
                    skip,
                    descending,
                    inclusive_end: true,
                    ..Default::default()
                })
                .await?;
            print_json(&serde_json::to_value(&response).unwrap(), cli.pretty);
        }

        Commands::Find {
            path,
            selector,
            fields,
            sort,
            limit,
            skip,
            db_name,
        } => {
            let db = open_existing_db(&path, db_name.as_deref());
            let selector: serde_json::Value = serde_json::from_str(&selector).map_err(|e| {
                rouchdb::RouchError::BadRequest(format!("invalid selector JSON: {}", e))
            })?;

            let sort = sort
                .map(|s| {
                    serde_json::from_str::<Vec<rouchdb::SortField>>(&s).map_err(|e| {
                        rouchdb::RouchError::BadRequest(format!("invalid sort JSON: {}", e))
                    })
                })
                .transpose()?;

            let fields = fields.map(|f| f.split(',').map(|s| s.trim().to_string()).collect());

            let response = db
                .find(FindOptions {
                    selector,
                    fields,
                    sort,
                    limit,
                    skip,
                })
                .await?;

            print_json(
                &serde_json::json!({
                    "docs": response.docs,
                }),
                cli.pretty,
            );
        }

        Commands::Changes {
            path,
            since,
            limit,
            include_docs,
            descending,
            db_name,
        } => {
            let db = open_existing_db(&path, db_name.as_deref());
            let response = db
                .changes(ChangesOptions {
                    since: since.into(),
                    limit,
                    include_docs,
                    descending,
                    ..Default::default()
                })
                .await?;
            print_json(&serde_json::to_value(&response).unwrap(), cli.pretty);
        }

        Commands::Dump { path, db_name } => {
            let db = open_existing_db(&path, db_name.as_deref());
            let all = db
                .all_docs(AllDocsOptions {
                    include_docs: true,
                    inclusive_end: true,
                    ..Default::default()
                })
                .await?;

            let docs: Vec<&serde_json::Value> =
                all.rows.iter().filter_map(|row| row.doc.as_ref()).collect();
            print_json(&serde_json::to_value(&docs).unwrap(), cli.pretty);
        }

        Commands::Replicate {
            source,
            target,
            selector,
            source_name,
            target_name,
        } => {
            let source_db = open_source_or_target(&source, source_name.as_deref(), true);
            let target_db = open_source_or_target(&target, target_name.as_deref(), false);

            let selector_value = selector
                .map(|s| {
                    serde_json::from_str::<serde_json::Value>(&s).map_err(|e| {
                        rouchdb::RouchError::BadRequest(format!("invalid selector JSON: {}", e))
                    })
                })
                .transpose()?;

            let opts = ReplicationOptions {
                filter: selector_value.map(rouchdb::ReplicationFilter::Selector),
                ..Default::default()
            };

            let result = source_db.replicate_to_with_opts(&target_db, opts).await?;

            print_json(
                &serde_json::json!({
                    "ok": result.ok,
                    "docs_read": result.docs_read,
                    "docs_written": result.docs_written,
                }),
                cli.pretty,
            );
        }

        Commands::Compact { path, db_name } => {
            let db = open_existing_db(&path, db_name.as_deref());
            db.compact().await?;
            print_json(&serde_json::json!({"ok": true}), cli.pretty);
        }

        Commands::Put {
            path,
            doc_id,
            body,
            rev,
            force,
            db_name,
        } => {
            let db = open_db(&path, db_name.as_deref());
            let data: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
                rouchdb::RouchError::BadRequest(format!("invalid JSON body: {}", e))
            })?;

            let effective_rev = if rev.is_some() {
                rev
            } else if force {
                // Only a genuinely missing document should fall through to a
                // create; surface any other error instead of silently creating.
                match db.get(&doc_id).await {
                    Ok(doc) => doc.rev.map(|r| r.to_string()),
                    Err(rouchdb::RouchError::NotFound(_)) => None,
                    Err(e) => return Err(e),
                }
            } else {
                None
            };

            let result = if let Some(rev) = effective_rev {
                db.update(&doc_id, &rev, data).await?
            } else {
                db.put(&doc_id, data).await?
            };
            check_doc_result(&result)?;

            print_json(
                &serde_json::json!({
                    "ok": result.ok,
                    "id": result.id,
                    "rev": result.rev,
                }),
                cli.pretty,
            );
        }

        Commands::Delete {
            path,
            doc_id,
            rev,
            db_name,
        } => {
            let db = open_existing_db(&path, db_name.as_deref());
            let result = db.remove(&doc_id, &rev).await?;
            check_doc_result(&result)?;
            print_json(
                &serde_json::json!({
                    "ok": result.ok,
                    "id": result.id,
                    "rev": result.rev,
                }),
                cli.pretty,
            );
        }

        Commands::Post {
            path,
            body,
            db_name,
        } => {
            let db = open_db(&path, db_name.as_deref());
            let data: serde_json::Value = serde_json::from_str(&body).map_err(|e| {
                rouchdb::RouchError::BadRequest(format!("invalid JSON body: {}", e))
            })?;

            let result = db.post(data).await?;
            check_doc_result(&result)?;
            print_json(
                &serde_json::json!({
                    "ok": result.ok,
                    "id": result.id,
                    "rev": result.rev,
                }),
                cli.pretty,
            );
        }

        Commands::Import {
            path,
            file,
            db_name,
        } => {
            let db = open_db(&path, db_name.as_deref());
            let content = std::fs::read_to_string(&file).map_err(|e| {
                rouchdb::RouchError::BadRequest(format!("cannot read file '{}': {}", file, e))
            })?;
            let docs: Vec<serde_json::Value> = serde_json::from_str(&content).map_err(|e| {
                rouchdb::RouchError::BadRequest(format!("invalid JSON in '{}': {}", file, e))
            })?;

            let mut imported = 0u64;
            let mut errors = Vec::new();
            for doc in &docs {
                let id = match doc.get("_id").and_then(|v| v.as_str()) {
                    Some(id) => id.to_string(),
                    None => {
                        errors.push(serde_json::json!({
                            "error": "missing _id field",
                            "doc": doc,
                        }));
                        continue;
                    }
                };

                let mut data = doc.clone();
                // Strip _id and _rev from the body — put() handles them
                if let Some(obj) = data.as_object_mut() {
                    obj.remove("_id");
                    obj.remove("_rev");
                }

                match db.put(&id, data).await {
                    Ok(ref r) if !r.ok => {
                        let reason = r
                            .reason
                            .as_deref()
                            .or(r.error.as_deref())
                            .unwrap_or("document update conflict");
                        errors.push(serde_json::json!({
                            "id": id,
                            "error": reason,
                        }));
                    }
                    Ok(_) => imported += 1,
                    Err(e) => {
                        errors.push(serde_json::json!({
                            "id": id,
                            "error": e.to_string(),
                        }));
                    }
                }
            }

            print_json(
                &serde_json::json!({
                    "ok": errors.is_empty(),
                    "imported": imported,
                    "total": docs.len(),
                    "errors": errors,
                }),
                cli.pretty,
            );

            // Exit non-zero if any document failed, like the other write
            // commands (put/post/delete).
            if !errors.is_empty() {
                return Err(rouchdb::RouchError::BadRequest(format!(
                    "{} of {} documents failed to import",
                    errors.len(),
                    docs.len()
                )));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_credentials_masks_passwords_only() {
        assert_eq!(
            redact_credentials("error sending request for url (http://ad%FFmin:s3cret@h:1/db)"),
            "error sending request for url (http://ad%FFmin:***@h:1/db)"
        );
        assert_eq!(
            redact_credentials("a https://u:p@ss@h/x and http://h/y?q=a@b and http://v@h"),
            "a https://u:***@h/x and http://h/y?q=a@b and http://v@h"
        );
        assert_eq!(redact_credentials("no url here"), "no url here");
    }

    #[test]
    fn percent_encode_userinfo_escapes_reserved_bytes() {
        assert_eq!(percent_encode_userinfo("alice"), "alice");
        assert_eq!(
            percent_encode_userinfo("p@ss:w/rd %"),
            "p%40ss%3Aw%2Frd%20%25"
        );
        assert_eq!(percent_encode_userinfo("ñ"), "%C3%B1");
    }
}
