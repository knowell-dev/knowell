; CSS rule sets and block at-rules. At-rules without a @name capture are
; named by their prelude, e.g. "@media (max-width: 600px)".
(rule_set (selectors) @name (block) @body) @definition.rule
(media_statement (block) @body) @definition.rule
(supports_statement (block) @body) @definition.rule
(keyframes_statement (keyframes_name) @name (keyframe_block_list) @body) @definition.rule
(at_rule (block) @body) @definition.rule
