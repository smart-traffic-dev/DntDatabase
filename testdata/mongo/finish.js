// Healthcheck step 4 (as root), after `mongoimport` loaded orders.json. The view is created
// last: its presence is what marks the server as seeded.
db.getSiblingDB('dnt_fixture').createView('paid_orders', 'orders', [{ $match: { status: 'paid' } }]);
