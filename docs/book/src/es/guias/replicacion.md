# Replicacion

RouchDB implementa el protocolo de replicacion de CouchDB, permitiendo sincronizacion bidireccional entre bases de datos locales y remotas.

## Conceptos basicos

La replicacion copia cambios de una base de datos **fuente** a una base de datos **destino**. Es incremental: solo transfiere los documentos que han cambiado desde la ultima sincronizacion.

## Replicacion basica

### Push (local a remoto)

```rust
use rouchdb::Database;

let local = Database::open("mydb.redb", "mydb")?;
let remote = Database::http("http://admin:password@localhost:5984/mydb");

let result = local.replicate_to(&remote).await?;
println!("Docs leidos: {}", result.docs_read);
println!("Docs escritos: {}", result.docs_written);
```

### Pull (remoto a local)

```rust
let result = local.replicate_from(&remote).await?;
```

### Sync (bidireccional)

```rust
let (push, pull) = local.sync(&remote).await?;
println!("Push: {} escritos", push.docs_written);
println!("Pull: {} escritos", pull.docs_written);
```

## Configurar CouchDB

Para pruebas, usa Docker Compose:

```bash
docker compose up -d
```

Esto levanta CouchDB en `http://localhost:15984` con credenciales `admin:password`.

Para crear una base de datos en CouchDB:

```bash
curl -X PUT http://admin:password@localhost:15984/mydb
```

## Opciones de replicacion

```rust
use rouchdb::ReplicationOptions;

let result = local.replicate_to_with_opts(&remote, ReplicationOptions {
    batch_size: 50,
    ..Default::default()
}).await?;
```

| Campo | Tipo | Default | Descripcion |
|-------|------|---------|-------------|
| `batch_size` | `u64` | `100` | Numero de cambios a procesar por iteracion |
| `batches_limit` | `u64` | `10` | Maximo numero de lotes a buffear |
| `filter` | `Option<ReplicationFilter>` | `None` | Filtro opcional para replicacion selectiva |
| `since` | `Option<Seq>` | `None` | Secuencia inicial (en vez de leer checkpoint) |
| `checkpoint` | `bool` | `true` | Deshabilitar con `false` para no guardar/leer checkpoints |
| `live` | `bool` | `false` | Habilitar replicacion continua |
| `retry` | `bool` | `false` | Reintentar automaticamente en caso de error |
| `poll_interval` | `Duration` | `500ms` | Intervalo de sondeo en modo continuo |
| `back_off_function` | `Option<Box<dyn Fn(u32) -> Duration + Send + Sync>>` | `None` | Funcion de backoff para reintentos |

## Replicacion filtrada

Se puede replicar un subconjunto de documentos usando `ReplicationFilter`:

```rust
use rouchdb::{ReplicationOptions, ReplicationFilter};

// Por IDs de documento
let result = local.replicate_to_with_opts(&remote, ReplicationOptions {
    filter: Some(ReplicationFilter::DocIds(vec!["doc1".into(), "doc2".into()])),
    ..Default::default()
}).await?;

// Por selector Mango
let result = local.replicate_to_with_opts(&remote, ReplicationOptions {
    filter: Some(ReplicationFilter::Selector(serde_json::json!({"type": "invoice"}))),
    ..Default::default()
}).await?;

// Por closure personalizado
let result = local.replicate_to_with_opts(&remote, ReplicationOptions {
    filter: Some(ReplicationFilter::Custom(std::sync::Arc::new(|change| {
        change.id.starts_with("public:")
    }))),
    ..Default::default()
}).await?;
```

## Checkpoints

Los checkpoints permiten que la replicacion se reanude despues de una interrupcion. RouchDB guarda el progreso en documentos locales (`_local/{replication_id}`) en ambos lados.

Cuando la replicacion se reinicia:
1. Lee el checkpoint de ambos lados
2. Encuentra la ultima secuencia comun
3. Reanuda desde ahi (no necesita empezar desde cero)

## Resultado de la replicacion

```rust
let result = local.replicate_to(&remote).await?;

if result.ok {
    println!("Replicacion exitosa");
    println!("  Docs leidos: {}", result.docs_read);
    println!("  Docs escritos: {}", result.docs_written);
    println!("  Ultima secuencia: {:?}", result.last_seq);
} else {
    println!("Replicacion con errores:");
    for err in &result.errors {
        println!("  - {}", err);
    }
}
```

| Campo | Tipo | Descripcion |
|-------|------|-------------|
| `ok` | `bool` | `true` si no hubo errores |
| `docs_read` | `u64` | Cambios procesados |
| `docs_written` | `u64` | Documentos escritos en el destino |
| `errors` | `Vec<String>` | Errores individuales por documento |
| `last_seq` | `Seq` | Ultima secuencia alcanzada |

## Ejemplo completo

Sincronizar una base de datos local redb con CouchDB:

```rust
use rouchdb::Database;

#[tokio::main]
async fn main() -> rouchdb::Result<()> {
    // Base de datos local persistente
    let local = Database::open("app.redb", "app")?;

    // CouchDB remoto
    let remote = Database::http("http://admin:password@localhost:5984/app");

    // Agregar datos localmente (funciona offline)
    local.put("nota:1", serde_json::json!({
        "titulo": "Mi primera nota",
        "contenido": "Hola desde RouchDB!"
    })).await?;

    // Cuando hay conexion, sincronizar
    let (push, pull) = local.sync(&remote).await?;

    println!("Sincronizacion completa:");
    println!("  Push: {} docs", push.docs_written);
    println!("  Pull: {} docs", pull.docs_written);

    Ok(())
}
```

## Replicacion con eventos

Usa `replicate_to_with_events()` para recibir eventos de progreso durante la replicacion:

```rust
use rouchdb::ReplicationEvent;

let (result, mut rx) = local.replicate_to_with_events(
    &remote,
    ReplicationOptions::default(),
).await?;

while let Ok(event) = rx.try_recv() {
    match event {
        ReplicationEvent::Active => println!("Replicacion iniciada"),
        ReplicationEvent::Change { docs_read } => {
            println!("Progreso: {} docs leidos", docs_read);
        }
        ReplicationEvent::Complete(r) => {
            println!("Completado: {} escritos", r.docs_written);
        }
        ReplicationEvent::Error(msg) => println!("Error: {}", msg),
        ReplicationEvent::Paused => println!("Esperando cambios..."),
    }
}
```

## Replicacion continua (live)

La replicacion continua se ejecuta en segundo plano, sondeando periodicamente por nuevos cambios. Equivalente a `{ live: true }` en PouchDB.

```rust
use rouchdb::{ReplicationOptions, ReplicationEvent};

let (mut rx, handle) = local.replicate_to_live(&remote, ReplicationOptions {
    live: true,
    poll_interval: std::time::Duration::from_millis(500),
    retry: true,
    ..Default::default()
});

// Procesar eventos en un loop
tokio::spawn(async move {
    while let Some(event) = rx.recv().await {
        match event {
            ReplicationEvent::Complete(r) => {
                println!("Lote completado: {} docs escritos", r.docs_written);
            }
            ReplicationEvent::Paused => {
                println!("Actualizado, esperando nuevos cambios...");
            }
            _ => {}
        }
    }
});

// Cancelar cuando sea necesario
handle.cancel();
```

El `ReplicationHandle` controla la replicacion:
- `handle.cancel()` detiene la replicacion.
- Si se descarta el handle (Drop), la replicacion tambien se cancela.

## Manejo de errores

- **Error de red**: la replicacion se detiene. El checkpoint guarda el progreso para reanudar despues.
- **Error de autenticacion** (401/403): la replicacion se detiene inmediatamente.
- **Error en documento individual**: se registra en `result.errors` pero la replicacion continua con los demas documentos.
- **Conflicto de checkpoint** (409): se reintenta con la ultima revision.
