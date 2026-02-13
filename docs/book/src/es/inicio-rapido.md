# Inicio Rapido

Esta guia te lleva por las funcionalidades principales de RouchDB en 5 minutos.

## Instalacion como libreria

Agrega RouchDB a tu proyecto:

```toml
[dependencies]
rouchdb = "0.3"
tokio = { version = "1", features = ["full"] }
serde_json = "1"
```

## Herramienta de linea de comandos

RouchDB incluye un CLI para inspeccionar y consultar bases de datos redb:

```bash
cargo install --path crates/rouchdb-cli
```

Ejemplos de uso:

```bash
# Informacion de la base de datos
rouchdb info mydb.redb

# Obtener un documento por ID
rouchdb get mydb.redb user:alice

# Listar todos los documentos
rouchdb all-docs mydb.redb --include-docs

# Consulta Mango
rouchdb find mydb.redb --selector '{"age": {"$gte": 30}}'

# Exportar todos los documentos como JSON
rouchdb dump mydb.redb --pretty

# Crear o actualizar un documento
rouchdb put mydb.redb user:alice '{"name":"Alice","age":30}'
rouchdb put mydb.redb user:alice '{"name":"Alice","age":31}' --rev 1-abc

# Upsert — obtiene el rev actual automaticamente, crea si no existe
rouchdb put mydb.redb user:alice '{"name":"Alice","age":32}' --force

# Crear un documento con ID auto-generado
rouchdb post mydb.redb '{"name":"Bob","age":25}'

# Eliminar un documento
rouchdb delete mydb.redb user:alice --rev 2-def

# Importar documentos desde un archivo JSON
rouchdb import mydb.redb docs.json

# Replicar a CouchDB
rouchdb replicate mydb.redb http://admin:password@localhost:5984/mydb
```

## Servidor HTTP

RouchDB incluye un servidor HTTP compatible con CouchDB con la interfaz web Fauxton:

```bash
# Instalar el servidor
cargo install --path crates/rouchdb-server

# Descargar Fauxton (opcional, para la interfaz web)
bash scripts/download-fauxton.sh

# Iniciar el servidor
rouchdb-server mydb.redb --port 5984

# Abrir Fauxton en el navegador
open http://localhost:5984/_utils/
```

El servidor expone una API REST compatible con CouchDB — documentos, consultas, feed de cambios, attachments, seguridad, design docs y mas — asi que cualquier cliente CouchDB (Fauxton, PouchDB, curl) puede conectarse.

## Crear una base de datos

```rust
use rouchdb::Database;

#[tokio::main]
async fn main() -> rouchdb::Result<()> {
    // En memoria (datos se pierden al soltar — ideal para pruebas)
    let db = Database::memory("mydb");

    // Persistente (almacenado en disco via redb)
    // let db = Database::open("mydb.redb", "mydb")?;

    // CouchDB remoto
    // let db = Database::http("http://admin:password@localhost:5984/mydb");

    Ok(())
}
```

## Crear y leer documentos

```rust
use rouchdb::Database;

#[tokio::main]
async fn main() -> rouchdb::Result<()> {
    let db = Database::memory("mydb");

    // Crear un documento
    let result = db.put("user:alice", serde_json::json!({
        "name": "Alice",
        "email": "alice@example.com",
        "age": 30
    })).await?;

    println!("Creado con rev: {}", result.rev.unwrap());

    // Leerlo de vuelta
    let doc = db.get("user:alice").await?;
    println!("Nombre: {}", doc.data["name"]); // "Alice"

    Ok(())
}
```

## Actualizar y eliminar

Cada actualizacion requiere la revision actual para prevenir conflictos:

```rust
use rouchdb::Database;

#[tokio::main]
async fn main() -> rouchdb::Result<()> {
    let db = Database::memory("mydb");

    // Crear
    let r1 = db.put("user:alice", serde_json::json!({"name": "Alice", "age": 30})).await?;
    let rev = r1.rev.unwrap();

    // Actualizar (debe proveer la rev actual)
    let r2 = db.update("user:alice", &rev, serde_json::json!({
        "name": "Alice",
        "age": 31
    })).await?;

    // Eliminar (debe proveer la rev actual)
    let rev2 = r2.rev.unwrap();
    db.remove("user:alice", &rev2).await?;

    Ok(())
}
```

## Consultas con Mango

Encuentra documentos que coincidan con un selector:

```rust
use rouchdb::{Database, FindOptions};

#[tokio::main]
async fn main() -> rouchdb::Result<()> {
    let db = Database::memory("mydb");

    db.put("alice", serde_json::json!({"name": "Alice", "age": 30})).await?;
    db.put("bob", serde_json::json!({"name": "Bob", "age": 25})).await?;
    db.put("carol", serde_json::json!({"name": "Carol", "age": 35})).await?;

    // Encontrar usuarios mayores de 28
    let result = db.find(FindOptions {
        selector: serde_json::json!({"age": {"$gte": 28}}),
        ..Default::default()
    }).await?;

    for doc in &result.docs {
        println!("{}: edad {}", doc["name"], doc["age"]);
    }
    // Alice: edad 30
    // Carol: edad 35

    Ok(())
}
```

## Sincronizar dos bases de datos

```rust
use rouchdb::Database;

#[tokio::main]
async fn main() -> rouchdb::Result<()> {
    let local = Database::memory("local");
    let remote = Database::memory("remote");

    // Agregar datos a cada lado
    local.put("doc1", serde_json::json!({"desde": "local"})).await?;
    remote.put("doc2", serde_json::json!({"desde": "remote"})).await?;

    // Sincronizacion bidireccional
    let (push, pull) = local.sync(&remote).await?;
    println!("Push: {} docs escritos", push.docs_written);
    println!("Pull: {} docs escritos", pull.docs_written);

    // Ambas bases de datos ahora tienen ambos documentos
    let info = local.info().await?;
    println!("Local tiene {} docs", info.doc_count); // 2

    Ok(())
}
```

## Siguientes pasos

- [Operaciones CRUD](./guias/crud.md) — guia completa de operaciones de documentos
- [Consultas](./guias/consultas.md) — selectores Mango y vistas map/reduce
- [Replicacion](./guias/replicacion.md) — sincronizacion con CouchDB
