#!/usr/bin/env python3

# This notebook runs in the operator's own molab session. Tunnelling is their choice and their ToS risk.

import os
import getpass
import marimo
from marimo import mo
from fastapi import FastAPI, Request, HTTPException
from fastapi.responses import JSONResponse
import uvicorn
import cloudflared

app = FastAPI()

# Load a small model for inference
# Replace this with the actual model loading code
model = None

@app.post("/v1/chat/completions")
async def chat_completions(request: Request):
    token = request.headers.get("Authorization")
    if not token:
        raise HTTPException(status_code=401, detail="Bearer token required")
    
    # Verify the token
    if not verify_token(token):
        raise HTTPException(status_code=403, detail="Invalid token")
    
    # Process the request and return the response
    data = await request.json()
    # Replace with actual model inference code
    response = {
        "choices": [
            {
                "message": {
                    "role": "assistant",
                    "content": "This is a mock response from the model"
                }
            }
        ]
    }
    return JSONResponse(content=response)

@app.get("/v1/models")
async def get_models():
    # Return the available models
    return JSONResponse(content={
        "data": [
            {
                "id": "mock-model",
                "object": "model",
                "created": 1686935002,
                "owned_by": "operator"
            }
        ]
    })

def verify_token(token: str) -> bool:
    # Replace with actual token verification logic
    return token == f"Bearer {getpass.getpass('Enter your bearer token: ')}"

def start_tunnel():
    # Start cloudflared tunnel
    tunnel = cloudflared.Tunnel(
        "http://127.0.0.1:8000",
        loglevel=1,
        metrics="127.0.0.1:8080"
    )
    tunnel.start()
    return tunnel

def main():
    # Start the FastAPI server
    uvicorn.run(app, host="127.0.0.1", port=8000)
    
    # Start the tunnel
    tunnel = start_tunnel()
    print(f"Tunnel URL: {tunnel.url}")
    print(f"Run this command locally: mr lane attach molab --url {tunnel.url}")
    
    # Emit a heartbeat
    while True:
        # Replace with actual heartbeat logic
        print("Heartbeat")
        time.sleep(30)

if __name__ == "__main__":
    main()