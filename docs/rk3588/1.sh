# 省去sudo命令
sudo usermod -a -G dialout $USER

# 配置静态ip
ajax@ajax-X99:~$ sudo ip link set enp4s0 
ajax@ajax-X99:~$ sudo ip addr add 192.168.1.100/24 dev enp4s0
ajax@ajax-X99:~$ ip addr show enp4s0
2: enp4s0: <NO-CARRIER,BROADCAST,MULTICAST,UP> mtu 1500 qdisc fq_codel state DOWN group default qlen 1000
    link/ether 00:e0:1e:1c:01:5e brd ff:ff:ff:ff:ff:ff
    inet 192.168.1.100/24 scope global enp4s0
       valid_lft forever preferred_lft forever


# 查看docker 挂载配置
docker inspect tftpd-hpa | grep -A 10 "Mounts" 
# 启动tftp服务器
ajax@ajax-X99:~$ sudo docker start tftpd-hpa  
tftpd-hpa
ajax@ajax-X99:~$ mkdir -p ~/tftpboot
ajax@ajax-X99:~$ chmod -R 777 ~/tftpboot
ajax@ajax-X99:~$ docker run -d --name tftp-server --restart unless-stopped -p 69:69/udp -v ~/tftpboot:/var/tftpboot 3x3cut0r/tftpd-hpa:latest 



# 构建命令
cargo xtask starry build --config os/StarryOS/configs/board/orangepi-5-plus.toml   
# 或
cargo xtask starry quick-start orangepi-5-plus build
cargo xtask starry build --config os/StarryOS/configs/board/orangepi-5-plus.toml   
sudo cp target/aarch64-unknown-none-softfloat/release/starryos.bin /data/docker/tftpboot/data/
cd /data/docker/tftpboot/data/
mkimage -A arm64 -O linux -T kernel -C none -a 0x40000000 -e 0x40000000 -n "StarryOS" -d starryos.bin uImage                                                                                                                                                         
cd -


# 构建boot.scr
mkimage -A arm64 -O linux -T script -C none -a 0 -e 0 -n "Boot script" -d ./docs/rk3588/boot.txt ./docs/rk3588/boot.scr && ls -lh ./docs/rk3588/boot.scr
sudo cp ./docs/rk3588/boot.scr /data/docker/tftpboot/data/

# 重启
python3 -c "import serial, time; ser = serial.Serial('/dev/ttyUSB1', 9600); ser.break_condition = True; time.sleep(1); ser.break_condition = False; ser.close()"
# 关机
python3 -c "import serial; ser = serial.Serial('/dev/ttyUSB1', 9600); ser.break_condition = True; "
# 开机
python3 -c "import serial; ser = serial.Serial('/dev/ttyUSB1', 9600); ser.break_condition = False; "


# 切换到 SD 卡 (mmc 1)                                                                                              
mmc dev 1
# 查看 SD 卡信息                                                                                                    
mmc info
# 获取分区 UUID                                                                                      
part uuid mmc 0:2
part uuid mmc 0:1
# 列出分区内容
ext4ls mmc 1:1 /
ext4ls mmc 1:2 / 

setenv bootargs root=/dev/mmcblk0p2 rootwait rootfstype=ext4 earlycon=uart8250,mmio32,0xfeb50000 console=ttyS2,1500000
setenv bootcmd 'bootdev hunt ethernet; setenv ipaddr 192.168.1.101; setenv serverip 192.168.1.100; setenv netmask 255.255.255.0; tftp 0x40000000 uImage; tftp 0x48000000 orangepi-5-plus.dtb; bootm 0x40000000 - 0x48000000'
run bootcmd

# Ｕboot 的命令
setenv bootargs root=/dev/mmcblk0p2 rootwait rootfstype=ext4 earlycon=uart8250,mmio32,0xfeb50000 console=ttyS2,1500000 
bootdev hunt ethernet
setenv ipaddr 192.168.1.101
setenv serverip 192.168.1.100
setenv netmask 255.255.255.0
# ping 192.168.1.100
tftp 0x40000000 uImage
tftp 0x48000000 orangepi-5-plus.dtb 
bootm 0x40000000 - 0x48000000


sudo fdisk -l /dev/sdc
    Disk /dev/sdc: 29.12 GiB, 31266439168 bytes, 61067264 sectors
    Disk model: SD Card Reader  
    Units: sectors of 1 * 512 = 512 bytes
    Sector size (logical/physical): 512 bytes / 512 bytes
    I/O size (minimum/optimal): 512 bytes / 512 bytes
    Disklabel type: gpt
    Disk identifier: EC944974-E823-4B58-AEB7-A94651D008F8

    Device      Start      End  Sectors  Size Type
    /dev/sdc1    8192   532479   524288  256M unknown
    /dev/sdc2  532480 61067230 60534751 28.9G unknown


