import os
from binance.client import Client

# Read from ../.env manually
env_vars = {}
with open('../.env', 'r') as f:
    for line in f:
        line = line.strip()
        if line and '=' in line and not line.startswith('#'):
            k, v = line.split('=', 1)
            v = v.strip('"\'')
            env_vars[k] = v

api_key = env_vars.get('BINANCE_API_KEY')
api_secret = env_vars.get('BINANCE_SECRET_KEY')

client = Client(api_key, api_secret)

# Get open orders
orders = client.get_open_orders(symbol='ETHUSDC')
print("Open Orders:", orders)

# Cancel open orders if any
for order in orders:
    print("Canceling order:", order['orderId'])
    client.cancel_order(symbol='ETHUSDC', orderId=order['orderId'])

# Get balance
eth_balance = client.get_asset_balance(asset='ETH')
print("ETH Balance:", eth_balance)

free_eth = float(eth_balance['free'])
print("Free ETH:", free_eth)

if free_eth > 0:
    try:
        # Check lot size
        info = client.get_symbol_info('ETHUSDC')
        step_size = None
        for filt in info['filters']:
            if filt['filterType'] == 'LOT_SIZE':
                step_size = float(filt['stepSize'])
                break
        
        # Format qty to step_size
        import math
        precision = int(-math.log10(step_size)) if step_size else 5
        qty_str = "{:0.0{}f}".format(free_eth, precision)
        
        print("Selling:", qty_str, "ETH")
        # Sell
        res = client.order_market_sell(symbol='ETHUSDC', quantity=qty_str)
        print("Sell Response:", res)
    except Exception as e:
        print("Error selling:", e)

