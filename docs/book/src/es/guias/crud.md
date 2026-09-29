# Operaciones CRUD

Esta guia cubre las operaciones fundamentales de documentos en RouchDB.

## Crear un documento

Usa `put` para crear un documento con un ID especifico:

```rust
use rouchdb::Database;

let db = Database::memory("mydb");

let result = db.put("user:1", serde_json::json!({
    "name": "Alice",
    "email": "alice@example.com"
})).await?;

assert!(result.ok);
println!("Rev: {}", result.rev.unwrap()); // "1-abc123..."
```

El resultado contiene:
- `ok` — `true` si la operacion fue exitosa
- `id` — el ID del documento
- `rev` — la cadena de revision asignada

## Leer un documento

```rust
let doc = db.get("user:1").await?;

println!("ID: {}", doc.id);
println!("Rev: {}", doc.rev.unwrap());
println!("Datos: {}", doc.data);
println!("Nombre: {}", doc.data["name"]); // "Alice"
```

Si el documento no existe, se retorna `RouchError::NotFound`.

## Actualizar un documento

Para actualizar, debes proveer la revision actual. Esto previene conflictos de escritura:

```rust
// Leer primero para obtener la revision actual
let doc = db.get("user:1").await?;
let rev = doc.rev.unwrap().to_string();

// Actualizar con la revision actual
let result = db.update("user:1", &rev, serde_json::json!({
    "name": "Alice Smith",
    "email": "alice.smith@example.com"
})).await?;

println!("Nueva rev: {}", result.rev.unwrap()); // "2-def456..."
```

Si la revision proporcionada no coincide con la actual, se retorna `RouchError::Conflict`.

## Eliminar un documento

La eliminacion es un "soft delete" — marca el documento como eliminado pero mantiene el historial de revisiones:

```rust
let doc = db.get("user:1").await?;
let rev = doc.rev.unwrap().to_string();

let result = db.remove("user:1", &rev).await?;
assert!(result.ok);

// Intentar leer ahora retorna NotFound
let err = db.get("user:1").await;
assert!(err.is_err());
```

## Operaciones en lote

`bulk_docs` escribe multiples documentos en una sola operacion:

```rust
use rouchdb::{Document, BulkDocsOptions};
use std::collections::HashMap;

let docs = vec![
    Document {
        id: "user:1".into(),
        rev: None,
        deleted: false,
        data: serde_json::json!({"name": "Alice"}),
        attachments: HashMap::new(),
    },
    Document {
        id: "user:2".into(),
        rev: None,
        deleted: false,
        data: serde_json::json!({"name": "Bob"}),
        attachments: HashMap::new(),
    },
];

let results = db.bulk_docs(docs, BulkDocsOptions::new()).await?;

for r in &results {
    println!("{}: ok={}", r.id, r.ok);
}
```

## Listar todos los documentos

`all_docs` lista documentos con opciones de paginacion y filtrado:

```rust
use rouchdb::AllDocsOptions;

// Todos los documentos con sus datos
let response = db.all_docs(AllDocsOptions {
    include_docs: true,
    ..AllDocsOptions::new()
}).await?;

println!("Total: {} documentos", response.total_rows);

for row in &response.rows {
    // `key` es el ID del documento; `rev()`, su revision ganadora.
    println!("{}: rev {}", row.key, row.rev().unwrap_or_default());
    if let Some(ref doc) = row.doc {
        println!("  datos: {}", doc);
    }
}
```

### Paginacion y rangos

```rust
// Documentos del 10 al 20
let page = db.all_docs(AllDocsOptions {
    skip: 10,
    limit: Some(10),
    ..AllDocsOptions::new()
}).await?;

// Solo documentos con IDs que empiezan con "user:"
let users = db.all_docs(AllDocsOptions {
    start_key: Some("user:".into()),
    end_key: Some("user:\u{ffff}".into()),
    include_docs: true,
    ..AllDocsOptions::new()
}).await?;

// Documentos especificos por ID: una fila por clave, en orden. Una clave
// sin documento da una fila de error (`row.is_error()`, sin `id` ni `value`)
// y un documento borrado, una fila con `row.is_deleted()` y sin `doc`.
let specific = db.all_docs(AllDocsOptions {
    keys: Some(vec!["user:1".into(), "user:2".into()]),
    include_docs: true,
    ..AllDocsOptions::new()
}).await?;
```

## Manejo de errores

```rust
use rouchdb::RouchError;

match db.get("no-existe").await {
    Ok(doc) => println!("Encontrado: {}", doc.data),
    Err(RouchError::NotFound(_)) => println!("Documento no encontrado"),
    Err(e) => eprintln!("Error inesperado: {}", e),
}

// Manejar conflictos de actualizacion
match db.update("user:1", "rev-incorrecta", serde_json::json!({})).await {
    Ok(_) => println!("Actualizado"),
    Err(RouchError::Conflict) => println!("Conflicto: el documento fue modificado"),
    Err(e) => eprintln!("Error: {}", e),
}
```
