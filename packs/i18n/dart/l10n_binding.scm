; final l10n = AppLocalizations.of(context)!;  /  final l10n = AppLocalizations.of(context);
(initialized_variable_definition
  name: (identifier) @name
  value: [
    (call_expression
      function: (member_expression object: (identifier) @class property: (identifier) @_of))
    (null_assertion_expression
      value: (call_expression
        function: (member_expression object: (identifier) @class property: (identifier) @_of)))
  ]
  (#match? @class "Localizations$")
  (#eq? @_of "of"))
