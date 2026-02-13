# RouchDB

**Una base de datos de documentos local-first para Rust con soporte del protocolo de replicacion de CouchDB.**

RouchDB es el equivalente en Rust de [PouchDB](https://pouchdb.com/) — te da un almacen local de documentos JSON que se sincroniza bidireccionalmente con [Apache CouchDB](https://couchdb.apache.org/) y servidores compatibles.

## Por que RouchDB?

- **No existe un equivalente en Rust.** Crates como `couch_rs` proveen clientes HTTP para CouchDB, pero ninguno ofrece almacenamiento local con replicacion. RouchDB llena este vacio.
- **Disenado para funcionar sin conexion.** Tu aplicacion funciona sin red. Cuando vuelve la conectividad, RouchDB sincroniza los cambios automaticamente.
- **Compatibilidad total con CouchDB.** Implementa el protocolo de replicacion de CouchDB, el modelo de arbol de revisiones y el lenguaje de consultas Mango.

## Caracteristicas

- **Operaciones CRUD** — `put`, `get`, `update`, `remove`, `bulk_docs`, `all_docs`
- **Consultas Mango** — `$eq`, `$gt`, `$in`, `$regex`, `$or` y mas de 15 operadores
- **Vistas map/reduce** — closures de Rust con reduces integrados `Sum`, `Count`, `Stats`
- **Feed de cambios** — consulta unica y streaming en vivo de mutaciones de documentos
- **Replicacion** — push, pull y sincronizacion bidireccional con CouchDB
- **Resolucion de conflictos** — algoritmo determinista de ganador, utilidades de deteccion de conflictos
- **Almacenamiento flexible** — en memoria, persistente (redb) o remoto (HTTP)
- **Servidor HTTP compatible con CouchDB** — navega bases de datos con Fauxton, usa cualquier cliente CouchDB
- **Rust puro** — sin dependencias de C, compila en todas las plataformas donde Rust compila

## Casos de uso

- Aplicaciones de escritorio con [Tauri](https://tauri.app/) que necesitan sincronizacion offline
- Herramientas CLI que almacenan datos localmente y sincronizan con un servidor
- Servicios backend que replican entre instancias de CouchDB
- Cualquier aplicacion Rust que necesite una base de datos de documentos local

## Ejemplo rapido

```rust
use rouchdb::Database;

#[tokio::main]
async fn main() -> rouchdb::Result<()> {
    let db = Database::memory("mydb");

    // Crear un documento
    let result = db.put("user:alice", serde_json::json!({
        "name": "Alice",
        "age": 30
    })).await?;

    // Leerlo de vuelta
    let doc = db.get("user:alice").await?;
    println!("{}: {}", doc.id, doc.data);

    Ok(())
}
```

Listo para empezar? Ve a [Inicio Rapido](./inicio-rapido.md).
