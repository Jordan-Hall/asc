; Project-owned tags for Rust (SPEC 23.2.1). The role and kind come from the capture name
; (def or ref, then the kind); name marks the identifier. No name capture: the node is the name.
(function_item name: (identifier) @name) @def.function
(function_signature_item name: (identifier) @name) @def.function
(struct_item name: (type_identifier) @name) @def.struct
(union_item name: (type_identifier) @name) @def.struct
(enum_item name: (type_identifier) @name) @def.enum
(enum_variant name: (identifier) @name) @def.variant
(trait_item name: (type_identifier) @name) @def.trait
(type_item name: (type_identifier) @name) @def.type
(mod_item name: (identifier) @name) @def.module
(const_item name: (identifier) @name) @def.constant
(static_item name: (identifier) @name) @def.constant
(macro_definition name: (identifier) @name) @def.macro
(field_declaration name: (field_identifier) @name) @def.field
(impl_item type: (type_identifier) @name) @def.impl
(impl_item type: (generic_type type: (type_identifier) @name)) @def.impl

(call_expression function: (identifier) @name) @ref.call
(call_expression function: (field_expression field: (field_identifier) @name)) @ref.call
(call_expression function: (scoped_identifier name: (identifier) @name)) @ref.call
(macro_invocation macro: (identifier) @name) @ref.call
(type_identifier) @ref.type
