# social-graph — Building and querying a social graph

A schema definition for a social graph with User / Project / Document, along with JSON AST query examples.

## Contents

```
social-graph/
├── schema.graph               # Schema definition
├── query_user_projects.json   # Query: get the Projects owned by a User
├── query_project_authors.json # Query: get the active owner Users of a Project
└── run.sh                     # Script that runs the full sequence of operations
```

## Run

```bash
bash examples/social-graph/run.sh
```

## Schema

```
node User        -- A user
node Project     -- A project
node Document    -- A document (has a Vector<384> embedding field)

edge OWNS<T: User>   -- A User owns a Project (with a generic bound)
edge AUTHORED        -- A User authored a Document
edge BELONGS_TO      -- A Document belongs to a Project
```

## Query examples

### Get the non-archived Projects owned by a User

```bash
chiffon query --db app.chiffon --json query_user_projects.json
```

JSON AST structure:

```json
{
  "start": { "type": "Node", "label": "User", "key": "id", "value": "user_alice" },
  "steps": [
    { "action": "OutEdges", "label": "OWNS" },
    { "action": "OutNodes", "label": "Project",
      "filter": { "property": "isArchived", "operator": "Equals", "value": false } }
  ],
  "collect": { "type": "Nodes", "properties": ["id", "title"] }
}
```

### Get the active owner Users of a Project

```bash
chiffon query --db app.chiffon --json query_project_authors.json
```

JSON AST structure:

```json
{
  "start": { "type": "Node", "label": "Project", "key": "id", "value": "project_alpha" },
  "steps": [
    { "action": "InEdges", "label": "OWNS" },
    { "action": "InNodes", "label": "User",
      "filter": { "property": "isActive", "operator": "Equals", "value": true } }
  ],
  "collect": { "type": "Nodes", "properties": ["id", "name"] }
}
```

## Traversal actions

| Action | Description |
|--------|-------------|
| `OutEdges` | Move to the edges going out of the current node |
| `InEdges`  | Move to the edges coming into the current node |
| `OutNodes` | Move to the destination node of the current edge |
| `InNodes`  | Move to the source node of the current edge |
| `Filter`   | Filter by a property condition |

## Filter operators

`Equals` / `NotEquals` / `GreaterThan` / `LessThan` / `GreaterThanOrEquals` / `LessThanOrEquals` / `Contains`
