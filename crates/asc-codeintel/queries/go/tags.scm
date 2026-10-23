; Project-owned tags for Go (SPEC 23.2.1).
(function_declaration name: (identifier) @name) @def.function
(method_declaration name: (field_identifier) @name) @def.method
(type_spec name: (type_identifier) @name type: (struct_type)) @def.struct
(type_spec name: (type_identifier) @name type: (interface_type)) @def.interface
(type_spec name: (type_identifier) @name) @def.type
(type_alias name: (type_identifier) @name) @def.type
(const_spec name: (identifier) @name) @def.constant
(var_spec name: (identifier) @name) @def.variable
(field_declaration name: (field_identifier) @name) @def.field
(method_elem name: (field_identifier) @name) @def.method

(call_expression function: (identifier) @name) @ref.call
(call_expression function: (selector_expression field: (field_identifier) @name)) @ref.call
(type_identifier) @ref.type
