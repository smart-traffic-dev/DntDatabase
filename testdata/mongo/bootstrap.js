// Healthcheck step 1 of mongo60 / mongo80 (testdata/docker-compose.test.yml), run without
// credentials. It relies on the localhost exception, so it only does anything until the first
// user exists: initiates the replica set (when DNT_REPLSET is set) and creates `root`.
const rsName = process.env.DNT_REPLSET;
if (rsName) {
  try {
    db.adminCommand({
      replSetInitiate: {
        _id: rsName,
        // The address clients on the Docker host use too (the port is published 1:1).
        members: [{ _id: 0, host: `127.0.0.1:${process.env.DNT_PORT}` }],
      },
    });
  } catch (e) {
    // AlreadyInitialized, or Unauthorized once `root` exists.
  }
  for (let i = 0; i < 120 && !db.hello().isWritablePrimary; i++) {
    sleep(500);
  }
}
db.getSiblingDB('admin').createUser({ user: 'root', pwd: 'dnt_test', roles: ['root'] });
