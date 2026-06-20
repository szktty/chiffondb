mod commands;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "chiffon", about = "ChiffonDB CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new database file
    Init {
        #[arg(long)]
        db: PathBuf,
    },
    /// Schema operations
    Schema {
        #[command(subcommand)]
        action: SchemaAction,
    },
    /// Node operations
    Node {
        #[command(subcommand)]
        action: NodeAction,
    },
    /// Edge operations
    Edge {
        #[command(subcommand)]
        action: EdgeAction,
    },
    /// Run a JSON AST query
    Query {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        json: String,
    },
    /// Show metadata about a database file
    Info {
        #[arg(long)]
        db: PathBuf,
    },
    /// Import from CSV or JSON
    Import {
        #[arg(long)]
        db: PathBuf,
        /// Format: csv or json
        #[arg(long)]
        format: String,
        /// Name of the node property referenced by an edge's :FROM/:TO
        #[arg(long, default_value = "id")]
        id_prop: String,
        /// Input file path (- for stdin)
        file: String,
    },
    /// Export to CSV or JSON
    Export {
        #[arg(long)]
        db: PathBuf,
        /// Format: csv or json
        #[arg(long)]
        format: String,
        /// Target type name (required for CSV; all records if omitted for JSON)
        #[arg(long)]
        r#type: Option<String>,
        /// Output file path (stdout if omitted)
        #[arg(long)]
        out: Option<String>,
    },
}

#[derive(Subcommand)]
enum SchemaAction {
    /// Apply a schema file to the database
    Apply {
        #[arg(long)]
        db: PathBuf,
        #[arg(long)]
        schema: PathBuf,
    },
    /// Show the schema currently applied to the database
    Show {
        #[arg(long)]
        db: PathBuf,
    },
}

#[derive(Subcommand)]
enum NodeAction {
    /// Insert a node
    Add {
        #[arg(long)]
        db: PathBuf,
        /// Node type name (as defined in the schema)
        #[arg(long)]
        r#type: String,
        /// Properties (JSON object)
        #[arg(long, default_value = "{}")]
        props: String,
    },
}

#[derive(Subcommand)]
enum EdgeAction {
    /// Insert an edge
    Add {
        #[arg(long)]
        db: PathBuf,
        /// Edge type name (as defined in the schema)
        #[arg(long)]
        r#type: String,
        /// Page ID of the source node
        #[arg(long)]
        from_page: u32,
        /// Slot ID of the source node
        #[arg(long)]
        from_slot: u16,
        /// Page ID of the destination node
        #[arg(long)]
        to_page: u32,
        /// Slot ID of the destination node
        #[arg(long)]
        to_slot: u16,
        /// Properties (JSON object)
        #[arg(long, default_value = "{}")]
        props: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init { db } => commands::init::run(&db),
        Command::Schema { action } => match action {
            SchemaAction::Apply { db, schema } => commands::schema::run_apply(&db, &schema),
            SchemaAction::Show { db } => commands::schema::run_show(&db),
        },
        Command::Node { action } => match action {
            NodeAction::Add { db, r#type, props } => commands::node::run_add(&db, &r#type, &props),
        },
        Command::Edge { action } => match action {
            EdgeAction::Add {
                db,
                r#type,
                from_page,
                from_slot,
                to_page,
                to_slot,
                props,
            } => commands::edge::run_add(
                &db, &r#type, from_page, from_slot, to_page, to_slot, &props,
            ),
        },
        Command::Query { db, json } => commands::query::run(&db, &json),
        Command::Info { db } => commands::info::run(&db),
        Command::Import {
            db,
            format,
            id_prop,
            file,
        } => commands::import::run(&db, &format, &id_prop, &file),
        Command::Export {
            db,
            format,
            r#type,
            out,
        } => commands::export::run(&db, &format, r#type.as_deref(), out.as_deref()),
    }
}
