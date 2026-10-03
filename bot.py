import os
from dotenv import load_dotenv
from alpaca.trading.client import TradingClient
from google import genai

# Load secrets from the .env file
load_dotenv()

ALPACA_KEY = os.getenv("ALPACA_API_KEY")
ALPACA_SECRET = os.getenv("ALPACA_SECRET_KEY")
GEMINI_KEY = os.getenv("GEMINI_API_KEY")

# Initialize Alpaca in Paper Trading mode
trading_client = TradingClient(ALPACA_KEY, ALPACA_SECRET, paper=True)

# Initialize Google GenAI / Vertex Client
ai_client = genai.Client(api_key=GEMINI_KEY)

print("Clients initialized successfully on Arch!")
