# JackLink - Portable Computing Gateway
# Copyright (c) 2025 Konstantin Zaremski
# Licensed under the MIT License

from setuptools import setup, find_packages

setup(
    name="jacklink",
    version="0.1.0",
    description="Portable computing gateway - browser-based environment over USB tether",
    author="Konstantin Zaremski",
    license="MIT",
    packages=find_packages(),
    include_package_data=True,
    package_data={
        "": ["static/**/*"],
    },
    install_requires=[
        "fastapi>=0.100.0",
        "uvicorn[standard]>=0.23.0",
        "wsgidav>=4.0.0",
        "python-multipart>=0.0.6",
        "aiofiles>=23.0.0",
        "rich>=13.0.0",
        "websockets>=11.0.0",
        "httpx>=0.25.0",
    ],
    entry_points={
        "console_scripts": [
            "jacklink=jacklink.__main__:run",
        ],
    },
    python_requires=">=3.11",
    classifiers=[
        "Development Status :: 3 - Alpha",
        "Environment :: Console",
        "Environment :: Web Environment",
        "Intended Audience :: Developers",
        "Intended Audience :: System Administrators",
        "License :: OSI Approved :: MIT License",
        "Operating System :: POSIX :: Linux",
        "Programming Language :: Python :: 3",
        "Programming Language :: Python :: 3.11",
        "Programming Language :: Python :: 3.12",
        "Topic :: System :: Networking",
        "Topic :: Utilities",
    ],
)
