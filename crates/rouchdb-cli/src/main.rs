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
    Replicate {
        /// Source: path to .redb file or CouchDB URL
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

fn open_source_or_target(path_or_url: &str, name: Option<&str>) -> Database {
    if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
        Database::http(path_or_url)
    } else {
        open_db(path_or_url, name)
    }
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
    let output = if pretty {
        serde_json::to_string_pretty(value).unwrap()
    } else {
        serde_json::to_string(value).unwrap()
    };
    println!("{}", output);
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = run(cli).await;
    if let Err(e) = result {
        eprintln!("Error: {}", e);
        process::exit(1);
    }
}

async fn run(cli: Cli) -> rouchdb::Result<()> {
    match cli.command {
        Commands::Info { path, db_name } => {
            let db = open_db(&path, db_name.as_deref());
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
            let db = open_db(&path, db_name.as_deref());
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
            let db = open_db(&path, db_name.as_deref());
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
            let db = open_db(&path, db_name.as_deref());
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
            let db = open_db(&path, db_name.as_deref());
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
            let db = open_db(&path, db_name.as_deref());
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
            let source_db = open_source_or_target(&source, source_name.as_deref());
            let target_db = open_source_or_target(&target, target_name.as_deref());

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
            let db = open_db(&path, db_name.as_deref());
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
                db.get(&doc_id).await.ok().and_then(|doc| {
                    doc.rev.map(|r| r.to_string())
                })
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
            let db = open_db(&path, db_name.as_deref());
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
            let docs: Vec<serde_json::Value> =
                serde_json::from_str(&content).map_err(|e| {
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
        }
    }

    Ok(())
}
