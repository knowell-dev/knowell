from flask import Blueprint, Flask

app = Flask(__name__)
bp = Blueprint("ledgers", __name__, url_prefix="/v1/ledgers")


@bp.route("/<int:ledger_id>", methods=["GET", "PATCH"])
def ledger(ledger_id):
    return {"id": ledger_id}


@app.route("/status")
def status():
    return "ok"


@bp.post("/close")
def close():
    return "", 204
