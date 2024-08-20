// make this file a markdown files

# ESP32C6 Treat Dispenser

# Treat Dispenser

This is a treat dispenser that uses an esp32c6 that is controlled via MQTT, in order to dispense a treat increment the integer value of the topic treats/dispense by 1.

the folowing environment variables are required to run the application set them in .cargo/config.toml:

SSID="ssid"

PASSWORD="wifi-password"

MQTT_USER="user-name"

MQTT_PASSWORD="password"

MQTT_HOST="192.168.1.x" 

You will need 12 6x3 magnets and 1 9g tower pro servo motor
this is the esp32c6 i used https://www.seeedstudio.com/Seeed-Studio-XIAO-ESP32C6-p-5884.html?srsltid=AfmBOopADoSuqkHCJxdfwXwqMwWBWb45EKbbi61cSO5zjooXyV4RdBel
the files for 3d printing and CAD are here
https://cad.onshape.com/documents/4cbb10cb1f9e13a4883ee0cb/w/051ddde997e1ed2f367a7828/e/24e8618560b2fb447e3dc53d?renderMode=0&uiState=66c51cd351106579865865f7
