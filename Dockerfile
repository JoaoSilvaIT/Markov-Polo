FROM python:3.11-slim

WORKDIR /app

# Install dependencies
COPY requirements.txt .
RUN pip install --no-cache-dir -r requirements.txt

# Copy the bot script
COPY binance_bot.py .

# Run the bot in unbuffered mode so logs appear instantly
CMD ["python", "-u", "binance_bot.py", "--loop", "15"]
