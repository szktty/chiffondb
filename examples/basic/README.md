# basic — Basic CLI operations

An example showing the basic workflow of ChiffonDB.

## Contents

```
basic/
├── schema.graph   # Schema definition (User / Project / OWNS)
└── run.sh         # Script that runs the full sequence of operations
```

## Run

```bash
bash examples/basic/run.sh
```

## Step-by-step

### 1. Create a database

```bash
chiffon init --db app.chiffon
```

### 2. Apply a schema

```bash
chiffon schema apply --db app.chiffon --schema schema.graph
```

### 3. Show the schema

```bash
chiffon schema show --db app.chiffon
```

### 4. Show database file info

```bash
chiffon info --db app.chiffon
```

Example output:

```
File:            app.chiffon
Version:         1
Page size:       4096 bytes
Total pages:     2
Topology pages:  63
Property pages:  2
Vector pages:    0 (reserved)
```
