; Project-owned tags for C# (SPEC 23.2.1). Upstream has no fields, properties or records.
(namespace_declaration name: (_) @name) @def.module
(file_scoped_namespace_declaration name: (_) @name) @def.module
(class_declaration name: (identifier) @name) @def.class
(record_declaration name: (identifier) @name) @def.class
(struct_declaration name: (identifier) @name) @def.struct
(interface_declaration name: (identifier) @name) @def.interface
(enum_declaration name: (identifier) @name) @def.enum
(enum_member_declaration name: (identifier) @name) @def.variant
(delegate_declaration name: (identifier) @name) @def.type
(method_declaration name: (identifier) @name) @def.method
(constructor_declaration name: (identifier) @name) @def.method
(local_function_statement name: (identifier) @name) @def.function
(property_declaration name: (identifier) @name) @def.property
(field_declaration (variable_declaration (variable_declarator name: (identifier) @name))) @def.field

(invocation_expression function: (identifier) @name) @ref.call
(invocation_expression function: (member_access_expression name: (identifier) @name)) @ref.call
(object_creation_expression type: (identifier) @name) @ref.call
