; t("key"), t(`key.${x}`), i18n.t("key"), i18next.t("key"), $t("key"), this.$t("key")
(call_expression
  function: (identifier) @_t
  arguments: (arguments . [(string) (template_string)] @key)
  (#any-of? @_t "t" "$t" "translate"))

(call_expression
  function: (member_expression property: (property_identifier) @_t)
  arguments: (arguments . [(string) (template_string)] @key)
  (#any-of? @_t "t" "$t"))
