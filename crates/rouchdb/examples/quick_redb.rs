use rouchdb::Database;

#[tokio::main]
async fn main() -> rouchdb::Result<()> {
    let path = "mydb.redb";

    let db = Database::open(path, "mydb")?;

    let r1 = db
        .put("hello", serde_json::json!({"msg": "it works!"}))
        .await?;
    println!("put rev: {}", r1.rev.unwrap());

    let doc = db.get("hello").await?;
    println!("{}", doc.data["msg"]);

    let info = db.info().await?;
    println!("doc_count: {}", info.doc_count);

    Ok(())
}
