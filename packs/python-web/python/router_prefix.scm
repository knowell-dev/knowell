; router = APIRouter(prefix="/v1/x")  /  bp = Blueprint("x", __name__, url_prefix="/v1/x")

(assignment
  left: (identifier) @router
  right: (call
    function: (identifier) @_c
    arguments: (argument_list
      (keyword_argument name: (identifier) @_k value: (string) @prefix)))
  (#any-of? @_c "APIRouter" "Blueprint")
  (#any-of? @_k "prefix" "url_prefix"))
