"""最小 FTP 测试服务器（pyftpdlib），支持 REST 断点续传。"""
import os
from pyftpdlib.authorizers import DummyAuthorizer
from pyftpdlib.handlers import FTPHandler
from pyftpdlib.servers import FTPServer

# FTP 根目录设为脚本所在目录（testdata），便于测试。
os.chdir(os.path.dirname(os.path.abspath(__file__)))

authorizer = DummyAuthorizer()
authorizer.add_user("user", "12345", ".", perm="elradfmwMT")
authorizer.add_anonymous(".")

handler = FTPHandler
handler.authorizer = authorizer

server = FTPServer(("127.0.0.1", 2121), handler)
print("FTP server on 127.0.0.1:2121, root =", os.getcwd())
server.serve_forever()