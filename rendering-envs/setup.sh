#!/bin/sh

# Prepares the environments for pandoc & vivliostyle

# Pandoc:
# Überprüfen, ob das Verzeichnis "pandoc" existiert
if [ -d "pandoc" ]; then
  # Initialer Wert für den Zähler
  count=1
  
  # Finde den nächsten verfügbaren Namen für pandoc-old-x
  while [ -d "pandoc-old-$count" ]; do
    count=$((count + 1))
  done
  
  # Verschiebe das vorhandene pandoc-Verzeichnis
  mv pandoc "pandoc-old-$count"
fi

# Erstelle das neue pandoc-Verzeichnis
mkdir pandoc

latest_release=$(curl --silent "https://api.github.com/repos/jgm/pandoc/releases/latest" | grep "browser_download_url.*linux-amd64.tar.gz" | cut -d '"' -f 4)

# Prüfen, ob eine URL gefunden wurde
if [ -z "$latest_release" ]; then
  echo "Error: couldn't extract download link to latest release."
  exit 1
fi

curl -L "$latest_release" -o pandoc/pandoc-latest.tar.gz

tar -xzf pandoc/pandoc-latest.tar.gz -C pandoc --strip-components=1

rm pandoc/pandoc-latest.tar.gz

mv pandoc/bin/pandoc pandoc/pandoc
rm -r pandoc/bin pandoc/share

echo "Finished preparing pandoc env."

# Weasyprint
# Step 1: Check if the weasyprint directory exists and rename it if necessary
if [ -d "weasyprint" ]; then
  count=1
  while [ -d "weasyprint-old-$count" ]; do
    count=$((count + 1))
  done
  mv weasyprint "weasyprint-old-$count"
fi

# Step 2: Create a new weasyprint directory
mkdir -p weasyprint
cd weasyprint || exit 1

python3 -m venv --copies venv
venv/bin/pip install weasyprint #nuitka
#venv/bin/nuitka --clang --standalone --static-libpython=yes --output-dir=../ --remove-output venv/bin/weasyprint
#cd ../
#rm -rf build
cd ../

# Vivliostyle

# Step 1: Check if the vivliostyle directory exists and rename it if necessary
if [ -d "vivliostyle" ]; then
  count=1
  while [ -d "vivliostyle-old-$count" ]; do
    count=$((count + 1))
  done
  mv vivliostyle "vivliostyle-old-$count"
fi

# Step 2: Create a new vivliostyle directory
mkdir vivliostyle

# Step 4: Download the latest stable version of Node.js using curl
NODE_VERSION=$(curl -sL https://nodejs.org/dist/latest/ | grep -oP 'node-v\K[0-9]+\.[0-9]+\.[0-9]+' | head -n 1)
NODE_TAR="node-v$NODE_VERSION-linux-x64.tar.xz"
NODE_URL="https://nodejs.org/dist/latest/$NODE_TAR"
mkdir vivliostyle/node-build

# Download the Node.js source code into the node-build subdirectory
curl -o "vivliostyle/node-build/$NODE_TAR" "$NODE_URL"

# Extract the downloaded archive into the node-build subdirectory
tar -xf "vivliostyle/node-build/$NODE_TAR" -C vivliostyle/node-build --strip-components=1

# Copy the newly created node binary to the vivliostyle directory
cp vivliostyle/node-build/bin/node vivliostyle/node

# Clean up by removing the node-build subdirectory
rm -rf vivliostyle/node-build

# Install Vivliostyle CLI in the vivliostyle directory using the new node binary
cd vivliostyle || exit 1
npm install @vivliostyle/cli

rm package.json
rm package-lock.json
echo "Finished preparing vivliostyle env."

cd ..
#Create a subdirectory for Chromium
mkdir vivliostyle/chromium

# Download the latest Chromium testing version using curl
CHROMIUM_VERSION=$(curl -sL https://www.googleapis.com/download/storage/v1/b/chromium-browser-snapshots/o/Linux_x64%2FLAST_CHANGE?alt=media)
CHROMIUM_TAR="chrome-linux-$CHROMIUM_VERSION.zip"
CHROMIUM_URL="https://www.googleapis.com/download/storage/v1/b/chromium-browser-snapshots/o/Linux_x64%2F$CHROMIUM_VERSION%2Fchrome-linux.zip?alt=media"

# Download Chromium testing version into the chromium subdirectory
curl -o "vivliostyle/chromium/$CHROMIUM_TAR" "$CHROMIUM_URL"

# Extract the downloaded Chromium archive into the chromium subdirectory
unzip -q "vivliostyle/chromium/$CHROMIUM_TAR" -d vivliostyle/chromium
mv vivliostyle/chromium/chrome-linux/* vivliostyle/chromium/
rmdir vivliostyle/chromium/chrome-linux

# Clean up by removing the Chromium zip file
rm "vivliostyle/chromium/$CHROMIUM_TAR"

echo "Finished preparing vivliostyle and chromium env."