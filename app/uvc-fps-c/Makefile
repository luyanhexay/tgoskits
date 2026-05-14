# Makefile for uvc-fps

YOLO_3RDPARTY = /home/ajax/Projects/Tg/yolo_testsuit/yolov8/3rdparty

CC ?= gcc
CFLAGS ?= -Wall -Wextra -O2
LDFLAGS ?= -luvc -lpthread

TARGET = uvc-fps
SRC = uvc-fps.c

# Prefix for libuvc installation (adjust if needed)
PREFIX ?= /usr/local
CFLAGS += -I$(PREFIX)/include
LDFLAGS += -L$(PREFIX)/lib

.PHONY: all clean aarch64

all: $(TARGET)

$(TARGET): $(SRC)
	$(CC) $(CFLAGS) -o $(TARGET) $(SRC) $(LDFLAGS)

clean:
	rm -f $(TARGET)



# Cross-compilation to ARM64 using local 3rdparty libraries
# All libraries are dynamically linked (board must have the .so files)
aarch64:
	@echo "=== Cross-compiling for ARM64 using local 3rdparty libraries ==="
	aarch64-linux-gnu-gcc \
		-Wall -Wextra -O2 \
		-I$(YOLO_3RDPARTY)/libuvc/include \
		-I$(YOLO_3RDPARTY)/libuvc/build/include \
		-I$(YOLO_3RDPARTY)/libusb-1.0.27/libusb \
		-o $(TARGET) \
		$(SRC) \
		-L$(YOLO_3RDPARTY)/libuvc/build \
		-L$(YOLO_3RDPARTY)/libusb-1.0.27/build/libusb/.libs \
		-luvc -lusb-1.0 -lpthread
	@echo ""
	@echo "Build complete. Checking binary:"
	@file $(TARGET)
	@echo ""
	@echo "Dynamic dependencies:"
	@aarch64-linux-gnu-readelf -d $(TARGET) | grep NEEDED || echo "  (run 'readelf' to see)"
