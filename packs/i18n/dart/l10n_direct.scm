; AppLocalizations.of(context)!.key / AppLocalizations.of(context).key
(member_expression
  object: [
    (null_assertion_expression
      value: (call_expression
        function: (member_expression object: (identifier) @_class property: (identifier) @_of)))
    (call_expression
      function: (member_expression object: (identifier) @_class property: (identifier) @_of))
  ]
  property: (identifier) @key
  (#match? @_class "Localizations$")
  (#eq? @_of "of"))
