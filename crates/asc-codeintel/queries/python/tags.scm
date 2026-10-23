; Project-owned tags for Python (SPEC 23.2.1).
(function_definition name: (identifier) @name) @def.function
(class_definition name: (identifier) @name) @def.class
(module (expression_statement (assignment left: (identifier) @name)) @def.variable)
(class_definition body: (block (expression_statement (assignment left: (identifier) @name)) @def.field))

(call function: (identifier) @name) @ref.call
(call function: (attribute attribute: (identifier) @name)) @ref.call
