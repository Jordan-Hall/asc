; Project-owned tags for JavaScript (SPEC 23.2.1).
(function_declaration name: (identifier) @name) @def.function
(generator_function_declaration name: (identifier) @name) @def.function
(class_declaration name: (identifier) @name) @def.class
(method_definition name: (property_identifier) @name) @def.method
(field_definition property: (property_identifier) @name) @def.property
(variable_declarator name: (identifier) @name value: [(arrow_function) (function_expression)]) @def.function
(program (lexical_declaration (variable_declarator name: (identifier) @name) @def.variable))
(program (variable_declaration (variable_declarator name: (identifier) @name) @def.variable))
(program (export_statement (lexical_declaration (variable_declarator name: (identifier) @name) @def.variable)))

(call_expression function: (identifier) @name) @ref.call
(call_expression function: (member_expression property: (property_identifier) @name)) @ref.call
(new_expression constructor: (identifier) @name) @ref.call
