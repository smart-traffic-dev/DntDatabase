// Healthcheck step 2 (as root): the limited user, and an empty dnt_fixture for the import.
const admin = db.getSiblingDB('admin');
if (!admin.getUser('dnt')) {
  admin.createUser({
    user: 'dnt',
    pwd: 'dnt_test',
    roles: [{ role: 'readWrite', db: 'dnt_fixture' }],
  });
}
db.getSiblingDB('dnt_fixture').dropDatabase();
