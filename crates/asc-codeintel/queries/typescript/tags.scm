; Project-owned tags for TypeScript and TSX (SPEC 23.2.1).
(function_declaration name: (identifier) @name) @def.function
(generator_function_declaration name: (identifier) @name) @def.function
(function_signature name: (identifier) @name) @def.function
(class_declaration name: (type_identifier) @name) @def.class
(abstract_class_declaration name: (type_identifier) @name) @def.class
(interface_declaration name: (type_identifier) @name) @def.interface
(type_alias_declaration name: (type_identifier) @name) @def.type
(enum_declaration name: (identifier) @name) @def.enum
(internal_module name: (identifier) @name) @def.module
(method_definition name: (property_identifier) @name) @def.method
(method_signature name: (property_identifier) @name) @def.method
(abstract_method_signature name: (property_identifier) @name) @def.method
(public_field_definition name: (property_identifier) @name) @def.property
(property_signature name: (property_identifier) @name) @def.property
(variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @def.function
(program (lexical_declaration (variable_declarator name: (identifier) @name) @def.variable))
(program (export_statement (lexical_declaration (variable_declarator name: (identifier) @name) @def.variable)))

(call_expression function: (identifier) @name) @ref.call
(call_expression function: (member_expression property: (property_identifier) @name)) @ref.call
(new_expression constructor: (identifier) @name) @ref.call
(type_identifier) @ref.type
