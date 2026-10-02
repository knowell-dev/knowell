; Ruby declarations. `def` inside a class or module becomes a method.

(class name: [(constant) (scope_resolution)] @name body: (_) @body) @definition.class
(class name: [(constant) (scope_resolution)] @name) @definition.class
(module name: [(constant) (scope_resolution)] @name body: (_) @body) @definition.module
(module name: [(constant) (scope_resolution)] @name) @definition.module

(method name: (_) @name body: (_) @body) @definition.function
(method name: (_) @name) @definition.function
(singleton_method name: (_) @name body: (_) @body) @definition.function
(singleton_method name: (_) @name) @definition.function

(assignment left: (constant) @name) @definition.constant
