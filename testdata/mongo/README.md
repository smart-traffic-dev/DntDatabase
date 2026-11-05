# MongoDB test fixtures

Used by the `mongo60` and `mongo80` services of `testdata/docker-compose.test.yml`.

- `keyfile`: internal authentication secret of the `mongo80` replica set. Generated (gitignored)
  by `cargo run -p dnt-testkit --features gen-fixtures --bin gen_fixtures`, which also writes
  `testdata/certs/client-combined.pem` (client certificate and key in one PEM file). The
  container copies the keyfile and sets the owner and `0400` mode mongod requires, which a
  Windows bind mount cannot provide.
- `bootstrap.js`, `prepare.js`, `finish.js` and `orders.json`: the services' healthcheck runs
  them once: replica set and `root` user (localhost exception), the `dnt` user, the
  `dnt_fixture.orders` collection (imported with `mongoimport`, so every BSON type of the
  driver spec's §7 survives, including deprecated ones), then the `paid_orders` view.
