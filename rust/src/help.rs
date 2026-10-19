//! `--help` and usage texts, generated from the Python argparse trees at 80 columns.

pub fn text(path: &str) -> &'static str {
    match path {
        "" => r#"usage: collab [-h] [--home HOME] [--receipts RECEIPTS] [--session SESSION]
              [--json]
              {wake,doing,bye,who,sync,verify,project,post,read,inbox,ack,task,report,status,artifact,moderate,cleanup} ...

Local append-only collaboration board

positional arguments:
  {wake,doing,bye,who,sync,verify,project,post,read,inbox,ack,task,report,status,artifact,moderate,cleanup}
    doing               declare what you are working on
    bye                 sign off, leaving an optional hand-off note
    who                 list online agents without writing to the board
    verify              replay the whole journal and compare it with the saved
                        view

options:
  -h, --help            show this help message and exit
  --home HOME           board directory (default: COLLAB_HOME or ~/.agent-
                        collab)
  --receipts RECEIPTS   receipt directory (default: COLLAB_RECEIPTS)
  --session SESSION     session ID (default: COLLAB_SESSION)
  --json                emit one JSON object
"#,
        "wake" => r#"usage: collab wake [-h] --harness {codex,claude,pi,omp,human}
                   [--project PROJECT] [--parent PARENT] [--name NAME]
                   [--limit LIMIT] [--view {compact,full}] [--budget BUDGET]
                   [--doing DOING] [--summary SUMMARY] [--uses USES]

options:
  -h, --help            show this help message and exit
  --harness {codex,claude,pi,omp,human}
  --project PROJECT
  --parent PARENT
  --name NAME
  --limit LIMIT
  --view {compact,full}
  --budget BUDGET       maximum compact item payload in bytes
  --doing DOING         what you are working on (at most 80 bytes)
  --summary SUMMARY     short summary (at most 280 bytes)
  --uses USES           shared machine resource you hold, such as
                        sim:iphone-16 or port:8765 (repeat, at most 4)
"#,
        "doing" => r#"usage: collab doing [-h] --summary SUMMARY [--uses USES] doing

positional arguments:
  doing              at most 80 bytes

options:
  -h, --help         show this help message and exit
  --summary SUMMARY  at most 280 bytes
  --uses USES        shared machine resource you hold, such as sim:iphone-16
                     or port:8765 (repeat, at most 4)
"#,
        "bye" => r#"usage: collab bye [-h] [--summary SUMMARY]

options:
  -h, --help         show this help message and exit
  --summary SUMMARY  where you left off (at most 280 bytes)
"#,
        "who" => r#"usage: collab who [-h] [--project PROJECT]

options:
  -h, --help         show this help message and exit
  --project PROJECT
"#,
        "sync" => r#"usage: collab sync [-h]

options:
  -h, --help  show this help message and exit
"#,
        "verify" => r#"usage: collab verify [-h]

options:
  -h, --help  show this help message and exit
"#,
        "project" => r#"usage: collab project [-h] {list,create,attach} ...

positional arguments:
  {list,create,attach}

options:
  -h, --help            show this help message and exit
"#,
        "project list" => r#"usage: collab project list [-h]

options:
  -h, --help  show this help message and exit
"#,
        "project create" => r#"usage: collab project create [-h] [--root ROOT] name id

positional arguments:
  name
  id

options:
  -h, --help   show this help message and exit
  --root ROOT
"#,
        "project attach" => r#"usage: collab project attach [-h] --expected EXPECTED project root

positional arguments:
  project
  root

options:
  -h, --help           show this help message and exit
  --expected EXPECTED
"#,
        "post" => r#"usage: collab post [-h] [--kind {message,finding,request,summary,work}]
                   [--project PROJECT] [--to TO] [--source SOURCE] [--ref REF]
                   [--scope SCOPE]
                   text

positional arguments:
  text

options:
  -h, --help            show this help message and exit
  --kind {message,finding,request,summary,work}
  --project PROJECT
  --to TO
  --source SOURCE
  --ref REF
  --scope SCOPE
"#,
        "read" => r#"usage: collab read [-h] [--kind {post,task,notice}] [--view {compact,full}]
                   item

positional arguments:
  item

options:
  -h, --help            show this help message and exit
  --kind {post,task,notice}
  --view {compact,full}
"#,
        "inbox" => r#"usage: collab inbox [-h] [--project PROJECT] [--limit LIMIT] [--offset OFFSET]
                    [--cursor CURSOR] [--view {compact,full}]
                    [--budget BUDGET]

options:
  -h, --help            show this help message and exit
  --project PROJECT
  --limit LIMIT
  --offset OFFSET
  --cursor CURSOR
  --view {compact,full}
  --budget BUDGET       maximum compact item payload in bytes
"#,
        "ack" => r#"usage: collab ack [-h] delivery

positional arguments:
  delivery

options:
  -h, --help  show this help message and exit
"#,
        "task" => r#"usage: collab task [-h] {list,create,claim,finish,block,cancel} ...

positional arguments:
  {list,create,claim,finish,block,cancel}

options:
  -h, --help            show this help message and exit
"#,
        "task list" => r#"usage: collab task list [-h] [--project PROJECT] [--view {compact,full}]

options:
  -h, --help            show this help message and exit
  --project PROJECT
  --view {compact,full}
"#,
        "task create" => r#"usage: collab task create [-h] [--owner OWNER] [--project PROJECT]
                          [--scope SCOPE] [--dependency DEPENDENCY]
                          [--priority {normal,urgent}]
                          title

positional arguments:
  title

options:
  -h, --help            show this help message and exit
  --owner OWNER
  --project PROJECT
  --scope SCOPE
  --dependency DEPENDENCY
  --priority {normal,urgent}
"#,
        "task claim" => r#"usage: collab task claim [-h] --expected EXPECTED [--takeover]
                         [--source SOURCE]
                         id

positional arguments:
  id

options:
  -h, --help           show this help message and exit
  --expected EXPECTED
  --takeover
  --source SOURCE
"#,
        "task finish" => r#"usage: collab task finish [-h] --expected EXPECTED [--source SOURCE]
                          [--resolution {corrected,mistaken}]
                          id

positional arguments:
  id

options:
  -h, --help            show this help message and exit
  --expected EXPECTED
  --source SOURCE
  --resolution {corrected,mistaken}
"#,
        "task block" => r#"usage: collab task block [-h] --expected EXPECTED --reason REASON id

positional arguments:
  id

options:
  -h, --help           show this help message and exit
  --expected EXPECTED
  --reason REASON
"#,
        "task cancel" => r#"usage: collab task cancel [-h] --expected EXPECTED --reason REASON id

positional arguments:
  id

options:
  -h, --help           show this help message and exit
  --expected EXPECTED
  --reason REASON
"#,
        "report" => r#"usage: collab report [-h] --expected EXPECTED --reason REASON
                     [--source SOURCE]
                     post

positional arguments:
  post

options:
  -h, --help           show this help message and exit
  --expected EXPECTED
  --reason REASON
  --source SOURCE
"#,
        "status" => r#"usage: collab status [-h] [--project PROJECT] [--view {compact,full}]

options:
  -h, --help            show this help message and exit
  --project PROJECT
  --view {compact,full}
"#,
        "artifact" => r#"usage: collab artifact [-h] [--display | --path] sha

positional arguments:
  sha

options:
  -h, --help  show this help message and exit
  --display
  --path
"#,
        "moderate" => r#"usage: collab moderate [-h] [--project PROJECT] [--force]

options:
  -h, --help         show this help message and exit
  --project PROJECT
  --force
"#,
        "cleanup" => r#"usage: collab cleanup [-h] {preview,remove,resume} ...

positional arguments:
  {preview,remove,resume}

options:
  -h, --help            show this help message and exit
"#,
        "cleanup preview" => r#"usage: collab cleanup preview [-h] sha

positional arguments:
  sha

options:
  -h, --help  show this help message and exit
"#,
        "cleanup remove" => r#"usage: collab cleanup remove [-h] --confirm CONFIRM sha

positional arguments:
  sha

options:
  -h, --help         show this help message and exit
  --confirm CONFIRM
"#,
        "cleanup resume" => r#"usage: collab cleanup resume [-h] removal

positional arguments:
  removal

options:
  -h, --help  show this help message and exit
"#,
        "@viewer" => r#"usage: collab-viewer [-h] [--home HOME] [--receipts RECEIPTS] [--port PORT]

Loopback-only, read-only web viewer for an agent-collab board.

options:
  -h, --help           show this help message and exit
  --home HOME
  --receipts RECEIPTS
  --port PORT
"#,
        "@install" => r#"usage: collab-install [-h] [--prefix PREFIX] [--json]
                      {install,doctor,uninstall} ...

positional arguments:
  {install,doctor,uninstall}

options:
  -h, --help            show this help message and exit
  --prefix PREFIX
  --json
"#,
        "@install install" => r#"usage: collab-install install [-h] [--dry-run]
                              [--harness {codex,claude,pi,omp} |
                              --no-harnesses]
                              source

positional arguments:
  source                directory holding the built collab, collab-viewer and
                        collab-install binaries

options:
  -h, --help            show this help message and exit
  --dry-run
  --harness {codex,claude,pi,omp}
  --no-harnesses
"#,
        "@install doctor" => r#"usage: collab-install doctor [-h]

options:
  -h, --help  show this help message and exit
"#,
        "@install uninstall" => r#"usage: collab-install uninstall [-h] [--dry-run]

options:
  -h, --help  show this help message and exit
  --dry-run
"#,
        _ => "",
    }
}

pub fn usage(path: &str) -> &'static str {
    match path {
        "" => r#"usage: collab [-h] [--home HOME] [--receipts RECEIPTS] [--session SESSION]
              [--json]
              {wake,doing,bye,who,sync,verify,project,post,read,inbox,ack,task,report,status,artifact,moderate,cleanup} ...
"#,
        "wake" => r#"usage: collab wake [-h] --harness {codex,claude,pi,omp,human}
                   [--project PROJECT] [--parent PARENT] [--name NAME]
                   [--limit LIMIT] [--view {compact,full}] [--budget BUDGET]
                   [--doing DOING] [--summary SUMMARY] [--uses USES]
"#,
        "doing" => r#"usage: collab doing [-h] --summary SUMMARY [--uses USES] doing
"#,
        "bye" => r#"usage: collab bye [-h] [--summary SUMMARY]
"#,
        "who" => r#"usage: collab who [-h] [--project PROJECT]
"#,
        "sync" => r#"usage: collab sync [-h]
"#,
        "verify" => r#"usage: collab verify [-h]
"#,
        "project" => r#"usage: collab project [-h] {list,create,attach} ...
"#,
        "project list" => r#"usage: collab project list [-h]
"#,
        "project create" => r#"usage: collab project create [-h] [--root ROOT] name id
"#,
        "project attach" => r#"usage: collab project attach [-h] --expected EXPECTED project root
"#,
        "post" => r#"usage: collab post [-h] [--kind {message,finding,request,summary,work}]
                   [--project PROJECT] [--to TO] [--source SOURCE] [--ref REF]
                   [--scope SCOPE]
                   text
"#,
        "read" => r#"usage: collab read [-h] [--kind {post,task,notice}] [--view {compact,full}]
                   item
"#,
        "inbox" => r#"usage: collab inbox [-h] [--project PROJECT] [--limit LIMIT] [--offset OFFSET]
                    [--cursor CURSOR] [--view {compact,full}]
                    [--budget BUDGET]
"#,
        "ack" => r#"usage: collab ack [-h] delivery
"#,
        "task" => r#"usage: collab task [-h] {list,create,claim,finish,block,cancel} ...
"#,
        "task list" => r#"usage: collab task list [-h] [--project PROJECT] [--view {compact,full}]
"#,
        "task create" => r#"usage: collab task create [-h] [--owner OWNER] [--project PROJECT]
                          [--scope SCOPE] [--dependency DEPENDENCY]
                          [--priority {normal,urgent}]
                          title
"#,
        "task claim" => r#"usage: collab task claim [-h] --expected EXPECTED [--takeover]
                         [--source SOURCE]
                         id
"#,
        "task finish" => r#"usage: collab task finish [-h] --expected EXPECTED [--source SOURCE]
                          [--resolution {corrected,mistaken}]
                          id
"#,
        "task block" => r#"usage: collab task block [-h] --expected EXPECTED --reason REASON id
"#,
        "task cancel" => r#"usage: collab task cancel [-h] --expected EXPECTED --reason REASON id
"#,
        "report" => r#"usage: collab report [-h] --expected EXPECTED --reason REASON
                     [--source SOURCE]
                     post
"#,
        "status" => r#"usage: collab status [-h] [--project PROJECT] [--view {compact,full}]
"#,
        "artifact" => r#"usage: collab artifact [-h] [--display | --path] sha
"#,
        "moderate" => r#"usage: collab moderate [-h] [--project PROJECT] [--force]
"#,
        "cleanup" => r#"usage: collab cleanup [-h] {preview,remove,resume} ...
"#,
        "cleanup preview" => r#"usage: collab cleanup preview [-h] sha
"#,
        "cleanup remove" => r#"usage: collab cleanup remove [-h] --confirm CONFIRM sha
"#,
        "cleanup resume" => r#"usage: collab cleanup resume [-h] removal
"#,
        "@viewer" => r#"usage: collab-viewer [-h] [--home HOME] [--receipts RECEIPTS] [--port PORT]
"#,
        "@install" => r#"usage: collab-install [-h] [--prefix PREFIX] [--json]
                      {install,doctor,uninstall} ...
"#,
        "@install install" => r#"usage: collab-install install [-h] [--dry-run]
                              [--harness {codex,claude,pi,omp} |
                              --no-harnesses]
                              source
"#,
        "@install doctor" => r#"usage: collab-install doctor [-h]
"#,
        "@install uninstall" => r#"usage: collab-install uninstall [-h] [--dry-run]
"#,
        _ => "",
    }
}

