use rouchdb::Database;

#[tokio::main]
async fn main() -> rouchdb::Result<()> {
    let db = Database::memory("test");

    let result = db
        .put("hello", serde_json::json!({"msg": "it works!"}))
        .await?;
    assert!(result.ok);

    let doc = db.get("hello").await?;
    println!("{}", doc.data["msg"]); // "it works!"

    Ok(())
}
