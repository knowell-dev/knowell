from fastapi import APIRouter, FastAPI

router = APIRouter(prefix="/v1/vouchers", tags=["vouchers"])
app = FastAPI()


@router.get("/{voucher_id}")
def get_voucher(voucher_id: str):
    return {"id": voucher_id}


@router.post("", status_code=201)
def create_voucher():
    return {}


@app.delete("/v1/vouchers/{voucher_id}")
async def delete_voucher(voucher_id: str):
    return None
