# Built-in rule packs

The packs in this directory are bundled with `knowell-link` (embedded at build time) and
extract cross-project **contracts** — HTTP endpoints, event topics, RPCs, database tables,
environment variable **names** and i18n keys — from source code and contract documents. The
authoring guide (format, captures, normalisers, testing) is in
[`crates/knowell-link/README.md`](../crates/knowell-link/README.md).

Every pack has a `pack.toml` (name, version, languages, detection hints, rules, known limits),
query files under `<language>/*.scm`, and at least two positive (`tests/pos-*`) and one
negative (`tests/neg-*`) fixture whose expected extractions are checked by
`cargo test -p knowell-link --test packs`.

A pack is active for a project when one of its detection hints matches (a dependency in
`package.json`, `go.mod`, `pyproject.toml` / `requirements*.txt` / `Pipfile`,
`pubspec.yaml`, `Cargo.toml`, `pom.xml` / `build.gradle(.kts)`, `*.csproj`; a project file
glob), or for a single file when one of its import hints matches that file's imports. Packs
without hints are always active.

## Matrix

Roles: **P** producer (serves / publishes / writes), **C** consumer (calls / subscribes /
reads), **D** definition (contract document, migration, locale file, deployment config).
Evidence is `syntactic` unless marked *heuristic*; definitions from contract documents are
`contract_derived`.

### HTTP servers

| Pack | Languages | Extracts | Detection |
|---|---|---|---|
| `nestjs` | TypeScript, TSX | P endpoints: `@Controller(prefix)` + `@Get/@Post/@Put/@Patch/@Delete/@Options/@Head/@All` (other decorators may sit in between); C topics: `@EventPattern` / `@MessagePattern` (constants resolved) | `@nestjs/*` |
| `express` | TS, TSX, JS, JSX | P endpoints: `router.get("/x", handler)` and friends (Express, Fastify, Koa router), `fastify.route({ method, url })` | `express`, `fastify`, `koa-router` |
| `nextjs` | TS, TSX, JS, JSX | P endpoints from file conventions: app router `app/**/route.ts` exports `GET`/`POST`/... (route groups, `@slots`, `_private` folders handled), pages router `pages/api/**` default export (`*` method) | `next` |
| `go-http` | Go | P endpoints: `mux.HandleFunc("GET /x/{id}", h)` (Go 1.22 patterns), `http.Handle`, chi `r.Get`, gin `r.GET`, echo `e.PUT`, gin/echo `Group("/prefix")` prefixes in the same file | `net/http`, chi, gin, echo |
| `python-web` | Python | P endpoints: FastAPI `@router.get` with `APIRouter(prefix=...)`, Flask `@app.route(..., methods=[...])`, `@bp.post` with `Blueprint(url_prefix=...)` | `fastapi`, `flask`, `starlette`, `quart` |
| `spring` | Java, Kotlin | P endpoints: `@RestController` / `@Controller` classes, class `@RequestMapping` prefix, `@GetMapping`/`@PostMapping`/... and `@RequestMapping(value, method = RequestMethod.X)` (Java) | Spring Boot, `org.springframework.web.bind.annotation` |
| `aspnet` | C# | P endpoints: `[Route("api/[controller]")]` + `[HttpGet("{id}")]` ... (`[controller]` / `[action]` tokens), minimal APIs `app.MapGet("/x", ...)` | `Microsoft.AspNetCore*` |

### HTTP clients

| Pack | Languages | Extracts | Detection |
|---|---|---|---|
| `js-http-client` | TS, TSX, JS, JSX | C endpoints: `fetch(url, { method })`, axios / ky / got `client.get("/x")`, `axios({ method, url })`; *heuristic*: `request(base, "/x", { method })`-style wrappers and `this.get/post(...)` | always (fetch); axios-style rules need `axios`, `ky`, `got`, `ofetch`, `superagent` |
| `dart-http-client` | Dart | C endpoints: dio `dio.get('/x')` incl. generic `get<T>`, package:http `http.post(Uri.parse('...'))` | `dio`, `http`, `chopper` |
| `swift-urlsession` | Swift | C endpoints: `URL(string: "https://.../x/\(id)")` with the method from `request.httpMethod = "POST"` in the same function | `import Foundation` |
| `retrofit` | Kotlin, Java | C endpoints: `@GET("v1/x/{id}")`, `@POST`, ... on service interface methods | `retrofit2` |
| `go-http-client` | Go | C endpoints: `http.NewRequest(method, url, body)`, `NewRequestWithContext`, `http.Get/Post/Head` (URL concatenations and constants resolved) | `net/http` |
| `python-http-client` | Python | C endpoints: `requests.get(...)`, `httpx.post(...)`, `client/session.get("/x")`, f-strings | `requests`, `httpx`, `aiohttp` |

### Messaging

| Pack | Languages | Extracts | Detection |
|---|---|---|---|
| `kafka` | TS/JS, Go, Java, Python, Rust | kafkajs `producer.send({ topic })` P, `consumer.subscribe({ topic(s) })` C, *heuristic* `events.publish(TOPIC, data)` P; kafka-go `Writer{Topic}` / `Message{Topic}` / sarama `ProducerMessage{Topic}` P, `ReaderConfig{Topic, GroupTopics}` C, *heuristic* `newReader(brokers, "x")`, `Subscribe("x")`, `SubscribeTopics(...)` C; Spring `kafkaTemplate.send("x")` / `new ProducerRecord<>("x")` P, `@KafkaListener(topics = ...)` C; Python `producer.send/send_and_wait/produce("x")` P, `AIOKafkaConsumer("a", "b")` / `consumer.subscribe([...])` C; rdkafka `consumer.subscribe(&[...])` / `subscribe(TOPICS)` C, `FutureRecord::to("x")` P, `match msg.topic() { "x" => }` C | kafkajs, kafka-go, sarama, confluent-kafka(-go), aiokafka, kafka-python, rdkafka, spring-kafka |
| `rabbitmq` | TS/JS, Go, Python, Java | amqplib `channel.publish(ex, key)` / `sendToQueue(q)` P, `channel.consume(q)` C; amqp091 `ch.Publish(ex, key, ...)` P, `ch.Consume(q, ...)` C; pika `basic_publish(routing_key=)` P, `basic_consume(queue=)` C; Spring AMQP `convertAndSend(...)` P, `@RabbitListener(queues = ...)` C | amqplib, amqp091-go, streadway/amqp, pika, aio-pika, spring-amqp |
| `nats` | TS/JS, Go, Python | `nc.publish("subj")` P, `nc.subscribe("subj")`, Go `QueueSubscribe` / `ChanSubscribe` C | `nats`, `nats.go`, `nats-py` |
| `redis-pubsub` | TS/JS, Go, Python | `publish("channel", msg)` P, `subscribe("a", ...)` C | `ioredis`, `redis`, go-redis |

### RPC and contract documents

| Pack | Languages | Extracts | Detection |
|---|---|---|---|
| `grpc` | proto, Go, Python, TS/JS | D RPCs from `.proto` (`package.Service/Method`, request/response shape hash); Go `client := pb.NewXClient(conn)` then `client.Method(...)` C, methods of structs embedding `UnimplementedXServer` P; Python `stub = pb.XStub(ch)` then `stub.Method(...)` C, `class S(pb.XServicer)` methods P; grpc-js `new XClient(...)` calls C, `server.addService(XService, { method })` P | `*.proto` files, grpc deps |
| `api-specs` | YAML, JSON | D endpoints from OpenAPI 3 / Swagger 2 (paths x methods, server base path, operation schema hash with local `$ref`s); D topics from AsyncAPI 2/3 channels and per-event JSON Schema files (`title` = topic, fields, version) | always |

### Databases

| Pack | Languages | Extracts | Detection |
|---|---|---|---|
| `sql-migrations` | SQL | D tables: `CREATE TABLE` columns, `ALTER TABLE ADD / DROP / RENAME COLUMN`, `RENAME TO`, `DROP TABLE`, in migration order (Rust DDL tokenizer) | always |
| `sql-in-code` | Go, Python, TS/JS, Java, Kotlin, Rust, C# | *heuristic*: SQL in string literals — `FROM` / `JOIN` C (reads), `INSERT INTO` / `UPDATE` / `DELETE FROM` P (writes) | always |
| `typeorm` | TS, TSX | C tables with column sets: `@Entity(...)` classes, `@Column` / `@PrimaryColumn` / `@CreateDateColumn` ... properties (`name` option) | `typeorm` |
| `prisma` | Prisma | C tables with column sets: `model` blocks (`@@map`, `@map`, relations skipped) (Rust scanner) | always (`*.prisma`) |
| `gorm` | Go | C tables with column sets: structs with `gorm:` tags (`column:`, `-`), `TableName()` overrides | always (tag-based) |
| `python-orm` | Python | C tables with column sets: SQLAlchemy `__tablename__` + `mapped_column` / `Column`; Django `models.Model` fields, `Meta.db_table`, `ForeignKey` -> `<name>_id` | `sqlalchemy`, `sqlmodel`, `django` |

### Configuration, i18n, infrastructure

| Pack | Languages | Extracts | Detection |
|---|---|---|---|
| `env` | TS/JS, Go, Python, Rust, Java, Kotlin, C#, Dart, YAML | C env **names**: `process.env.X`, `import.meta.env.X`, `os.Getenv` / `LookupEnv`, `os.environ[...]` / `.get` / `os.getenv`, pydantic `BaseSettings` fields, `std::env::var` / `env!`, `System.getenv`, `Environment.GetEnvironmentVariable`, `String.fromEnvironment`; *heuristic* `getenv("X", ...)` / `required("X")` helpers; D names from docker-compose `environment:` and Kubernetes container `env:` | always |
| `i18n` | TS/JS, Dart, JSON | C keys: `t("k")`, `i18n.t`, `$t`, `<Trans i18nKey>`, Flutter `AppLocalizations.of(ctx)!.key` and `l10n.key`; D keys (with locale) from nested locale JSON (`locales/en.json`, `locales/en/ns.json`) and ARB files | i18n libraries, `flutter_localizations`, locale files |
| `infra` | YAML | D services with images and container ports from docker-compose and Kubernetes Deployments / StatefulSets / DaemonSets / Jobs / Services (same name = same service) | always |
| `constants` | TS/JS, Python, Go, Rust, Java, Kotlin, Dart, C# | bindings only: module-level string constants and string lists, used by other packs to resolve `TOPIC`, `BASE_URL`, ... | always |

## Known limits (summary)

Each `pack.toml` lists its limits in full; the most important ones:

- **No inter-procedural analysis.** A key passed through a function parameter
  (`publish(type)`, `fetch(baseUrl + path)`) is recorded as an *unresolved* use pointing at a
  placeholder node; it is never guessed. Module-level constants are resolved (same file
  first, then a constant defined in exactly one file).
- **Prefixes outside the file are not applied**: Nest global prefixes, `app.use("/api",
  router)`, `include_router(prefix=...)`, chi `Route`, nested gin groups, Spring
  `context-path`, client base URLs. `LinkOptions::path_prefixes` covers gateway prefixes.
- **Messaging topology is not modelled**: RabbitMQ exchanges / bindings, NATS wildcards and
  Kafka topic patterns are kept literally.
- **ORM naming strategies** other than each library's default are not applied; embedded /
  inherited columns are not added.
- **Lower-case SQL in code strings** is not recognised (to keep prose out).
- **Tables and i18n keys are workspace-wide names**: two databases with the same table name,
  or two apps with the same message key, share one contract node.
- **Rust, PHP, Ruby, Scala and C/C++ HTTP frameworks** have no built-in pack yet (Rust is
  covered for Kafka, env names and SQL strings).
