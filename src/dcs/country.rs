use crate::dcs::cvt_err;
use mlua::{prelude::*, Value};
use serde_derive::Serialize;

#[derive(Debug, Clone, Serialize)]
#[allow(non_camel_case_types)]
#[repr(u8)]
pub enum Country {
    ABKHAZIA = 18,
    AGGRESSORS = 7,
    ALGERIA = 70,
    ARGENTINA = 83,
    AUSTRALIA = 21,
    AUSTRIA = 23,
    BAHRAIN = 65,
    BELARUS = 24,
    BELGIUM = 11,
    BOLIVIA = 86,
    BRAZIL = 64,
    BULGARIA = 25,
    CANADA = 8,
    CHEZH_REPUBLIC = 26,
    CHILE = 63,
    CHINA = 27,
    CJTF_BLUE = 80,
    CJTF_RED = 81,
    CROATIA = 28,
    CUBA = 76,
    CYPRUS = 84,
    DENMARK = 13,
    ECUADOR = 90,
    EGYPT = 29,
    ETHIOPIA = 62,
    FINLAND = 30,
    FRANCE = 5,
    GDR = 78,
    GEORGIA = 16,
    GERMANY = 6,
    GHANA = 87,
    GREECE = 31,
    HONDURAS = 61,
    HUNGARY = 32,
    INDIA = 33,
    INDONESIA = 60,
    INSURGENTS = 17,
    IRAN = 34,
    IRAQ = 35,
    ISRAEL = 15,
    ITALIAN_SOCIAL_REPUBLIC = 69,
    ITALY = 20,
    JAPAN = 36,
    JORDAN = 59,
    KAZAKHSTAN = 37,
    KUWAIT = 71,
    LEBANON = 79,
    LIBYA = 58,
    MALAYSIA = 57,
    MEXICO = 56,
    MOROCCO = 55,
    NIGERIA = 88,
    NORTH_KOREA = 38,
    NORWAY = 12,
    OMAN = 73,
    PAKISTAN = 39,
    PERU = 89,
    PHILIPPINES = 54,
    POLAND = 40,
    PORTUGAL = 77,
    QATAR = 72,
    ROMANIA = 41,
    RUSSIA = 0,
    SAUDI_ARABIA = 42,
    SERBIA = 43,
    SLOVAKIA = 44,
    SLOVENIA = 85,
    SOUTH_AFRICA = 75,
    SOUTH_KOREA = 45,
    SOUTH_OSETIA = 19,
    SPAIN = 9,
    SUDAN = 53,
    SWEDEN = 46,
    SWITZERLAND = 22,
    SYRIA = 47,
    THAILAND = 52,
    THE_NETHERLANDS = 10,
    THIRDREICH = 66,
    TUNISIA = 51,
    TURKEY = 3,
    UK = 4,
    UKRAINE = 1,
    UNITED_ARAB_EMIRATES = 74,
    UN_PEACEKEEPERS = 82,
    USA = 2,
    USSR = 68,
    VENEZUELA = 50,
    VIETNAM = 49,
    YEMEN = 48,
    YUGOSLAVIA = 67,
}

impl<'lua> IntoLua<'lua> for Country {
    fn into_lua(self, _: &'lua Lua) -> LuaResult<Value<'lua>> {
        Ok(Value::Integer(self as i64))
    }
}

impl<'lua> FromLua<'lua> for Country {
    fn from_lua(value: Value<'lua>, lua: &'lua Lua) -> LuaResult<Self> {
        use Country::*;
        Ok(match u8::from_lua(value, lua)? {
            18 => ABKHAZIA,
            7 => AGGRESSORS,
            70 => ALGERIA,
            83 => ARGENTINA,
            21 => AUSTRALIA,
            23 => AUSTRIA,
            65 => BAHRAIN,
            24 => BELARUS,
            11 => BELGIUM,
            86 => BOLIVIA,
            64 => BRAZIL,
            25 => BULGARIA,
            8 => CANADA,
            26 => CHEZH_REPUBLIC,
            63 => CHILE,
            27 => CHINA,
            80 => CJTF_BLUE,
            81 => CJTF_RED,
            28 => CROATIA,
            76 => CUBA,
            84 => CYPRUS,
            13 => DENMARK,
            90 => ECUADOR,
            29 => EGYPT,
            62 => ETHIOPIA,
            30 => FINLAND,
            5 => FRANCE,
            78 => GDR,
            16 => GEORGIA,
            6 => GERMANY,
            87 => GHANA,
            31 => GREECE,
            61 => HONDURAS,
            32 => HUNGARY,
            33 => INDIA,
            60 => INDONESIA,
            17 => INSURGENTS,
            34 => IRAN,
            35 => IRAQ,
            15 => ISRAEL,
            69 => ITALIAN_SOCIAL_REPUBLIC,
            20 => ITALY,
            36 => JAPAN,
            59 => JORDAN,
            37 => KAZAKHSTAN,
            71 => KUWAIT,
            79 => LEBANON,
            58 => LIBYA,
            57 => MALAYSIA,
            56 => MEXICO,
            55 => MOROCCO,
            88 => NIGERIA,
            38 => NORTH_KOREA,
            12 => NORWAY,
            73 => OMAN,
            39 => PAKISTAN,
            89 => PERU,
            54 => PHILIPPINES,
            40 => POLAND,
            77 => PORTUGAL,
            72 => QATAR,
            41 => ROMANIA,
            0 => RUSSIA,
            42 => SAUDI_ARABIA,
            43 => SERBIA,
            44 => SLOVAKIA,
            85 => SLOVENIA,
            75 => SOUTH_AFRICA,
            45 => SOUTH_KOREA,
            19 => SOUTH_OSETIA,
            9 => SPAIN,
            53 => SUDAN,
            46 => SWEDEN,
            22 => SWITZERLAND,
            47 => SYRIA,
            52 => THAILAND,
            10 => THE_NETHERLANDS,
            66 => THIRDREICH,
            51 => TUNISIA,
            3 => TURKEY,
            4 => UK,
            1 => UKRAINE,
            74 => UNITED_ARAB_EMIRATES,
            82 => UN_PEACEKEEPERS,
            2 => USA,
            68 => USSR,
            50 => VENEZUELA,
            49 => VIETNAM,
            48 => YEMEN,
            67 => YUGOSLAVIA,
            _ => return Err(cvt_err("Country")),
        })
    }
}
