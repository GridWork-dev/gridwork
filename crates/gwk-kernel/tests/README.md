# PostgreSQL integration tests

The [compose file](docker-compose.yml) provides PostgreSQL 16 for `admin_init.rs`,
`admin_migrate.rs`, `perf.rs`, `receipt_figures.rs`, and the suites using the shared
`common/mod.rs` harness. Tests create their own databases and need a superuser
maintenance connection.

From the repository root:

```sh
docker compose -f crates/gwk-kernel/tests/docker-compose.yml up -d --wait
GWK_TEST_ADMIN_DATABASE_URL=postgres://postgres@localhost:55432/postgres \
  cargo test -p gwk-kernel -- --ignored
```

To run one suite, add `--test admin_init` before `-- --ignored`.

The container is named `gwk-test-pg`. An existing container with that name created
outside Compose must be stopped and removed before Compose can create it. This is
a disposable test database; do not point the harness at a database you need to keep.

Trust authentication requires no password. The published port must remain
`127.0.0.1:55432:5432`: omitting the host IP exposes a passwordless PostgreSQL
superuser on other host interfaces. Inspect the resolved configuration with:

```sh
docker compose -f crates/gwk-kernel/tests/docker-compose.yml config
```

For a shared test harness, leave the container running between test runs. To
discard its test data and recreate it, run both commands:

```sh
docker compose -f crates/gwk-kernel/tests/docker-compose.yml down --volumes
docker compose -f crates/gwk-kernel/tests/docker-compose.yml up -d --wait
```
