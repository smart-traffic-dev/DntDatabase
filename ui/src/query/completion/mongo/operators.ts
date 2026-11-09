// The words the MongoDB console completion offers, as the console grammar accepts them
// (`crates/driver-mongo/src/shell/parse.rs`). Snippet bodies use Monaco's syntax: `$0` is
// where the cursor lands, `\$` a literal dollar.

export interface Word {
  label: string;
  detail: string;
  /** A snippet body; without it the label is inserted as is. */
  snippet?: string;
}

const w = (label: string, detail: string, snippet?: string): Word => ({ label, detail, snippet });

/** The first word of a statement. */
export const STATEMENT_WORDS: readonly Word[] = [
  w('db', 'The current database'),
  w('use', 'Switch the database of the tab'),
  w('show', 'List databases or collections'),
];

/** `show <what>`. */
export const SHOW_TARGETS: readonly Word[] = [
  w('dbs', 'The databases'),
  w('databases', 'The databases'),
  w('collections', 'The collections of the database'),
  w('tables', 'The collections of the database'),
];

/** Methods of `db.` (with the two collection / database accessors). */
export const DB_METHODS: readonly Word[] = [
  w('getCollection', 'A collection by name', 'getCollection("$0")'),
  w('getSiblingDB', 'Another database', 'getSiblingDB("$0")'),
  w('getCollectionNames', 'The collection names', 'getCollectionNames()'),
  w('stats', 'Database statistics', 'stats()'),
  w('serverStatus', 'Server status', 'serverStatus()'),
  w('version', 'Server version', 'version()'),
  w('createCollection', 'Create a collection', 'createCollection("$0")'),
  w('dropDatabase', 'Drop the database', 'dropDatabase()'),
  w('runCommand', 'Run a database command', 'runCommand({$0})'),
  w('adminCommand', 'Run a command on admin', 'adminCommand({$0})'),
];

const SET = '{ \\$set: { $0 } }';

/** Methods of `db.<collection>.`. */
export const COLL_METHODS: readonly Word[] = [
  w('find', 'Documents matching a filter', 'find({$0})'),
  w('findOne', 'The first matching document', 'findOne({$0})'),
  w('countDocuments', 'Count matching documents', 'countDocuments({$0})'),
  w('estimatedDocumentCount', 'Count from metadata', 'estimatedDocumentCount()'),
  w('distinct', 'Distinct values of a field', 'distinct("$1", {$0})'),
  w('aggregate', 'Run a pipeline', 'aggregate([$0])'),
  w('getIndexes', 'The indexes', 'getIndexes()'),
  w('stats', 'Collection statistics', 'stats()'),
  w('insertOne', 'Insert a document', 'insertOne({$0})'),
  w('insertMany', 'Insert documents', 'insertMany([{$0}])'),
  w('updateOne', 'Update the first match', `updateOne({$1}, ${SET})`),
  w('updateMany', 'Update every match', `updateMany({$1}, ${SET})`),
  w('replaceOne', 'Replace the first match', 'replaceOne({$1}, {$0})'),
  w('deleteOne', 'Delete the first match', 'deleteOne({$0})'),
  w('deleteMany', 'Delete every match', 'deleteMany({$0})'),
  w('findOneAndUpdate', 'Update and return a document', `findOneAndUpdate({$1}, ${SET})`),
  w('findOneAndReplace', 'Replace and return a document', 'findOneAndReplace({$1}, {$0})'),
  w('findOneAndDelete', 'Delete and return a document', 'findOneAndDelete({$0})'),
  w('createIndex', 'Create an index', 'createIndex({ $1: 1 })'),
  w('createIndexes', 'Create indexes', 'createIndexes([{ $1: 1 }])'),
  w('dropIndex', 'Drop an index', 'dropIndex("$0")'),
  w('dropIndexes', 'Drop indexes', 'dropIndexes()'),
  w('renameCollection', 'Rename the collection', 'renameCollection("$0")'),
  w('drop', 'Drop the collection', 'drop()'),
];

/** Cursor modifiers after `find(…)` / `aggregate(…)`. */
export const MODIFIERS: readonly Word[] = [
  w('sort', 'Sort the documents', 'sort({$0})'),
  w('limit', 'At most n documents', 'limit($0)'),
  w('skip', 'Skip n documents', 'skip($0)'),
  w('project', 'Fields to return', 'project({$0})'),
  w('projection', 'Fields to return', 'projection({$0})'),
  w('maxTimeMS', 'Server time limit', 'maxTimeMS($0)'),
  w('count', 'Count the documents', 'count()'),
  w('toArray', 'All documents', 'toArray()'),
  w('pretty', 'Formatted output', 'pretty()'),
  w('explain', 'The query plan', 'explain($0)'),
];

/** Operators of a filter. */
export const QUERY_OPERATORS: readonly Word[] = [
  w('$eq', 'Equal to'),
  w('$ne', 'Not equal to'),
  w('$gt', 'Greater than'),
  w('$gte', 'Greater than or equal to'),
  w('$lt', 'Less than'),
  w('$lte', 'Less than or equal to'),
  w('$in', 'In an array of values'),
  w('$nin', 'Not in an array of values'),
  w('$exists', 'The field exists'),
  w('$type', 'BSON type of the field'),
  w('$regex', 'Matches a regular expression'),
  w('$and', 'All filters match'),
  w('$or', 'Any filter matches'),
  w('$nor', 'No filter matches'),
  w('$not', 'Negates an operator'),
  w('$elemMatch', 'An array element matches'),
  w('$size', 'Array length'),
  w('$all', 'Array holds every value'),
  w('$expr', 'Aggregation expression'),
];

/** Operators at the top level of an update document. */
export const UPDATE_OPERATORS: readonly Word[] = [
  w('$set', 'Set fields'),
  w('$unset', 'Remove fields'),
  w('$inc', 'Increment fields'),
  w('$push', 'Append to an array'),
  w('$pull', 'Remove from an array'),
  w('$addToSet', 'Add to an array once'),
  w('$rename', 'Rename fields'),
  w('$min', 'Keep the smaller value'),
  w('$max', 'Keep the larger value'),
  w('$mul', 'Multiply fields'),
  w('$currentDate', 'Set to the current date'),
];

/** Stages of an aggregation pipeline. */
export const STAGES: readonly Word[] = [
  w('$match', 'Filter documents'),
  w('$group', 'Group documents'),
  w('$project', 'Reshape documents'),
  w('$sort', 'Sort documents'),
  w('$limit', 'At most n documents'),
  w('$skip', 'Skip n documents'),
  w('$unwind', 'One document per array element'),
  w('$lookup', 'Join another collection'),
  w('$addFields', 'Add fields'),
  w('$set', 'Add fields'),
  w('$unset', 'Remove fields'),
  w('$count', 'Count documents'),
  w('$facet', 'Several pipelines at once'),
  w('$replaceRoot', 'Replace the document'),
  w('$sample', 'Random documents'),
  w('$out', 'Write to a collection'),
  w('$merge', 'Merge into a collection'),
  w('$unionWith', 'Add another collection'),
];

/** Value helpers the console understands. */
export const HELPERS: readonly Word[] = [
  w('ObjectId', 'ObjectId value', 'ObjectId("$0")'),
  w('ISODate', 'Date value', 'ISODate("$0")'),
  w('NumberLong', '64-bit integer', 'NumberLong($0)'),
  w('NumberInt', '32-bit integer', 'NumberInt($0)'),
  w('NumberDecimal', '128-bit decimal', 'NumberDecimal("$0")'),
  w('UUID', 'UUID value', 'UUID("$0")'),
  w('BinData', 'Binary value', 'BinData(${1:0}, "$0")'),
  w('Timestamp', 'Timestamp value', 'Timestamp(${1:0}, ${2:0})'),
  w('MinKey', 'Lowest key'),
  w('MaxKey', 'Highest key'),
];
