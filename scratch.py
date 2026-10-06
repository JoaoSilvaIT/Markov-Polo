import requests

def get_price(symbol):
    url = f"https://api.binance.com/api/v3/ticker/price?symbol={symbol}"
    return requests.get(url).json()['price']

print("Current ETH/USDC:", get_price("ETHUSDC"))
print("Current SOL/USDC:", get_price("SOLUSDC"))
