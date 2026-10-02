(use_declaration argument: (_) @import.source) @import
(extern_crate_declaration name: (identifier) @import.source) @import
; `mod name;` loads another file.
(mod_item name: (identifier) @import.source !body) @import
