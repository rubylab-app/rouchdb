use std::collections::HashMap;
use std::io::{self, BufWriter, Write};
use std::process;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use clap::{Parser, Subcommand};
use rouchdb::{
    AllDocsOptions, BulkDocsOptions, ChangesOptions, Database, Document, FindOptions, GetOptions,
    ReplicationOptions,
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
    ///
    /// Each document is exported at its winning revision, with its attachments
    /// inlined as base64 ("_attachments": {name: {content_type, data}}), which
    /// `import` restores. Revision history and conflicting revisions are not
    /// exported (a warning names the documents that have conflicts); use
    /// `replicate` to copy a database with its full history.
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
    ///
    /// Exits with status 1 if any document could not be replicated; the
    /// "errors" field of the output says which and why.
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
    ///
    /// Documents get fresh revisions (any "_rev" is ignored). Inline
    /// attachments ("_attachments": {name: {content_type, data: <base64>}}),
    /// as written by `dump`, are restored; attachment stubs without data are
    /// rejected. Exits with status 1 if any document fails to import.
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

/// Documents written per `bulk_docs` call by `import`.
const IMPORT_BATCH_SIZE: usize = 500;

/// An attachment carried inline (base64) by an imported document.
struct InlineAttachment {
    name: String,
    content_type: String,
    data: Vec<u8>,
}

/// Turn one element of an import file into a new document plus its inline
/// attachments, or the error to report for it. Any `_rev` is dropped:
/// imported docs get fresh revisions.
fn import_doc(
    mut value: serde_json::Value,
) -> Result<(Document, Vec<InlineAttachment>), serde_json::Value> {
    let id = match value.get("_id").and_then(|v| v.as_str()).map(String::from) {
        Some(id) if id.is_empty() => {
            return Err(serde_json::json!({
                "id": id,
                "error": rouchdb::RouchError::MissingId.to_string(),
            }));
        }
        Some(id) => id,
        None => {
            return Err(serde_json::json!({
                "error": "missing _id field",
                "doc": value,
            }));
        }
    };
    let mut raw_attachments = None;
    if let Some(obj) = value.as_object_mut() {
        obj.remove("_id");
        obj.remove("_rev");
        raw_attachments = obj.remove("_attachments");
    }

    // Attachments are stored with put_attachment once the doc is written.
    let attachment_error = |message: String| serde_json::json!({"id": id, "error": message});
    let mut attachments = Vec::new();
    match raw_attachments {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::Object(map)) => {
            for (name, meta) in map {
                let Some(data) = meta.get("data").and_then(|v| v.as_str()) else {
                    return Err(attachment_error(format!(
                        "attachment '{}' has no inline data",
                        name
                    )));
                };
                let data = BASE64.decode(data).map_err(|e| {
                    attachment_error(format!("attachment '{}': invalid base64: {}", name, e))
                })?;
                let content_type = meta
                    .get("content_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("application/octet-stream")
                    .to_string();
                attachments.push(InlineAttachment {
                    name,
                    content_type,
                    data,
                });
            }
        }
        Some(_) => {
            return Err(attachment_error("_attachments must be an object".into()));
        }
    }

    let doc = Document {
        id,
        rev: None,
        deleted: false,
        data: value,
        attachments: HashMap::new(),
    };
    Ok((doc, attachments))
}

/// Store the inline attachments of an imported document, chaining from the
/// revision its `bulk_docs` write returned.
async fn put_inline_attachments(
    db: &Database,
    id: &str,
    rev: Option<String>,
    attachments: Vec<InlineAttachment>,
) -> Result<(), String> {
    if attachments.is_empty() {
        return Ok(());
    }
    let mut rev = rev.ok_or_else(|| "no revision returned for the document".to_string())?;
    for att in attachments {
        let result = db
            .put_attachment(id, &att.name, &rev, att.data, &att.content_type)
            .await
            .map_err(|e| format!("attachment '{}': {}", att.name, e))?;
        match result.rev {
            Some(new_rev) if result.ok => rev = new_rev,
            _ => {
                let reason = result.reason.or(result.error).unwrap_or_default();
                return Err(format!("attachment '{}': {}", att.name, reason));
            }
        }
    }
    Ok(())
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
            let selector: serde_json::Value = parse_json(&selector, "invalid selector JSON")?;
            let sort = sort
                .map(|s| {
                    let what = "invalid sort JSON";
                    serde_json::from_value::<Vec<rouchdb::SortField>>(parse_json(&s, what)?)
                        .map_err(|e| json_error(e.into(), what))
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
            let all = db.all_docs(AllDocsOptions::new()).await?;

            let mut docs = Vec::with_capacity(all.rows.len());
            let mut conflicted = Vec::new();
            for row in all.rows {
                // all_docs bodies carry no attachments, so read each doc
                // (with its conflicts, to warn about them) and inline the
                // attachment data in the format `import` reads back.
                let doc = db
                    .get_with_opts(
                        &row.id,
                        GetOptions {
                            conflicts: true,
                            ..Default::default()
                        },
                    )
                    .await?;
                let mut json = doc.to_json();
                if let Some(obj) = json.as_object_mut() {
                    if obj.remove("_conflicts").is_some() {
                        conflicted.push(row.id);
                    }
                    if !doc.attachments.is_empty() {
                        let mut names: Vec<&String> = doc.attachments.keys().collect();
                        names.sort();
                        let mut attachments = serde_json::Map::new();
                        for name in names {
                            let data = db.get_attachment(&doc.id, name).await?;
                            attachments.insert(
                                name.clone(),
                                serde_json::json!({
                                    "content_type": doc.attachments[name].content_type,
                                    "data": BASE64.encode(data),
                                }),
                            );
                        }
                        obj.insert(
                            "_attachments".into(),
                            serde_json::Value::Object(attachments),
                        );
                    }
                }
                docs.push(json);
            }

            if !conflicted.is_empty() {
                eprintln!(
                    "warning: {} document(s) have conflicting revisions; only the winning \
                     revision was exported: {}",
                    conflicted.len(),
                    conflicted.join(", ")
                );
            }
            print_json(&serde_json::Value::Array(docs), cli.pretty);
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
                .map(|s| parse_json(&s, "invalid selector JSON"))
                .transpose()?;

            let opts = ReplicationOptions {
                filter: selector_value.map(rouchdb::ReplicationFilter::Selector),
                ..Default::default()
            };

            let result = source_db.replicate_to_with_opts(&target_db, opts).await?;
            let errors: Vec<String> = result
                .errors
                .iter()
                .map(|e| redact_credentials(e))
                .collect();

            print_json(
                &serde_json::json!({
                    "ok": result.ok,
                    "docs_read": result.docs_read,
                    "docs_written": result.docs_written,
                    "errors": errors,
                    "last_seq": result.last_seq,
                }),
                cli.pretty,
            );

            // Documents that were not replicated must fail the command, so
            // `rouchdb replicate a.redb $URL && rm a.redb` cannot lose data.
            if !result.ok {
                return Err(rouchdb::RouchError::DatabaseError(format!(
                    "replication incomplete: {} error(s), see \"errors\" in the output",
                    errors.len()
                )));
            }
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
            let data: serde_json::Value = parse_json(&body, "invalid JSON body")?;

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
            let data: serde_json::Value = parse_json(&body, "invalid JSON body")?;

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
            let text = std::fs::read(&file).map_err(|e| {
                rouchdb::RouchError::BadRequest(format!("cannot read file '{}': {}", file, e))
            })?;
            // The documents sit one level deep, in the top-level array.
            let docs: Vec<serde_json::Value> = rouchdb_core::json::from_input(&text, 1)
                .map_err(|e| json_error(e, &format!("invalid JSON in '{}'", file)))?;
            let total = docs.len();

            let mut imported = 0u64;
            // Errors carry the document's position so they are reported in
            // file order, whichever stage rejected the document.
            let mut errors: Vec<(usize, serde_json::Value)> = Vec::new();
            let mut docs = docs.into_iter().enumerate().peekable();
            while docs.peek().is_some() {
                let mut batch = Vec::with_capacity(IMPORT_BATCH_SIZE);
                let mut positions = Vec::with_capacity(IMPORT_BATCH_SIZE);
                for (index, value) in docs.by_ref().take(IMPORT_BATCH_SIZE) {
                    match import_doc(value) {
                        Ok((doc, attachments)) => {
                            positions.push((index, doc.id.clone(), attachments));
                            batch.push(doc);
                        }
                        Err(e) => errors.push((index, e)),
                    }
                }
                if batch.is_empty() {
                    continue;
                }

                // One transaction per batch instead of one per document.
                match db.bulk_docs(batch, BulkDocsOptions::new()).await {
                    Ok(results) => {
                        let mut results = results.into_iter();
                        for (index, id, attachments) in positions {
                            match results.next() {
                                Some(r) if r.ok => {
                                    match put_inline_attachments(&db, &id, r.rev, attachments).await
                                    {
                                        Ok(()) => imported += 1,
                                        Err(e) => errors.push((
                                            index,
                                            serde_json::json!({"id": id, "error": e}),
                                        )),
                                    }
                                }
                                Some(r) => {
                                    let reason = r
                                        .reason
                                        .as_deref()
                                        .or(r.error.as_deref())
                                        .unwrap_or("document update conflict");
                                    errors.push((
                                        index,
                                        serde_json::json!({"id": id, "error": reason}),
                                    ));
                                }
                                None => errors.push((
                                    index,
                                    serde_json::json!({"id": id, "error": "no write result"}),
                                )),
                            }
                        }
                    }
                    Err(e) => {
                        for (index, id, _) in positions {
                            errors.push((
                                index,
                                serde_json::json!({"id": id, "error": e.to_string()}),
                            ));
                        }
                    }
                }
            }
            errors.sort_by_key(|(index, _)| *index);
            let errors: Vec<serde_json::Value> = errors.into_iter().map(|(_, e)| e).collect();

            print_json(
                &serde_json::json!({
                    "ok": errors.is_empty(),
                    "imported": imported,
                    "total": total,
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
                    total
                )));
            }
        }
    }

    Ok(())
}

/// Parse JSON given on the command line. Documents may be nested as deeply
/// as the database stores them ([`rouchdb::MAX_NESTING_DEPTH`]); deeper
/// input gets the database's error, malformed input `what` and the reason.
fn parse_json(text: &str, what: &str) -> rouchdb::Result<serde_json::Value> {
    rouchdb_core::json::from_input(text.as_bytes(), 0).map_err(|e| json_error(e, what))
}

/// A JSON decoding error prefixed with `what`; a too-deep input keeps the
/// database's error.
fn json_error(error: rouchdb::RouchError, what: &str) -> rouchdb::RouchError {
    match error {
        rouchdb::RouchError::Json(e) => rouchdb::RouchError::BadRequest(format!("{}: {}", what, e)),
        other => other,
    }
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
